//! Terminal widget construction and high-level pane/tab orchestration.
//!
//! Process lifecycle and child-exit handling live in `process`, while VTE
//! signal adapters and output/activity tracking live in `signals`.

mod attach;
mod broadcast;
mod broker_pty;
mod process;
mod restore;
mod signals;
mod splits;

pub(crate) use self::broker_pty::BrokerHandle;

// Non-test parent code uses these items from submodules.
pub(crate) use self::attach::resolve_attached_session_pane;
pub use self::attach::{
    attach_session_async, detach_session_by_name, kill_session_by_name_with_hint_async,
    poll_dashboard_state,
};
use self::broadcast::connect_broadcast_input;
pub(crate) use self::broadcast::{
    active_selection_text, broadcast_target_count, capture_grid_rows, capture_last_terminal_lines,
    clipboard_ring_snapshot, copy_last_message_from_active_pane, copy_recent_output_for_pane,
    copy_recent_output_from_active_terminal, copy_ring_entry_to_clipboard,
    copy_vte_selection_and_record, install_clipboard_ring_state, join_logical_lines,
    last_agent_message_for_active_pane, paste_clipboard_to_active_scope, resolve_pane_send_target,
    send_bytes_to_pane, LastAgentMessageResolution, PaneSendTarget, PromptJump,
};
pub use self::broadcast::{build_send_to_pane_payload, SendToPaneMode};
#[cfg(any(test, feature = "harness", debug_assertions))]
pub use self::broadcast::{
    exercise_broadcast_commit_reentrancy_harness, BroadcastCommitHarnessResult,
};
pub(crate) use self::restore::{
    apply_tmux_kill_outcomes_before_removal, run_tmux_command_sync_result_without_diagnostics,
    ssh_command_target_index, ssh_option_takes_value, ssh_supports_remote_exec,
};
use self::restore::{
    build_restored_pane_tree, build_restored_tab_legend, remote_split_respawn, saved_cwd_to_path,
};
pub use self::restore::{
    plan_restored_spawns, poll_host_status, poll_tmux_metadata, AgentResumeOffer, AutoResumeAgents,
    PlannedRestoreSpawn, SessionRestoreResumePolicy,
};
use self::splits::resolved_target_pane_id;
pub use self::splits::{
    close_pane, detach_pane, open_split_pane, save_pane_tree_for_tab, split_pane,
    split_pane_with_command,
};
pub(crate) use self::splits::{close_pane_async, register_detached_pane, register_detached_tab};

// Test-only re-exports from submodules (accessed via `use super::item` in mod tests).
#[cfg(test)]
use self::attach::{
    collect_dashboard_targets, kill_session_by_name_with_hint_using,
    resolve_attach_target_tab_id_in_workspace, resolve_attached_session_pane_in_list,
    resolve_session_target, AttachSessionError, AttachedSessionPane,
};
#[cfg(test)]
use self::broadcast::{
    resolve_dispatch_target_ids, should_broadcast_commit, DispatchTabSnapshot, DispatchTargetId,
};
#[cfg(test)]
use self::restore::{
    apply_dashboard_poll_results, apply_tmux_kill_result, apply_tmux_kill_result_in_workspace,
    classify_tmux_kill_result, cleanup_tmux_backing_with_behavior, emit_probe_transition_event,
    restored_ssh_command, run_tmux_command, run_tmux_list_sessions_command_sync_result,
    should_record_probe_failure_diagnostic, ssh_command_host, tmux_reports_no_server,
    tmux_session_exists, DashboardPollOutcome, TmuxKillResult,
};

use gio::prelude::*;
use gtk::prelude::*;
use vte::prelude::*;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Once, OnceLock};
use std::time::Duration;

const BRANCH_LABEL_CACHE_CAPACITY: usize = 128;

#[derive(Default)]
struct BranchLabelCache {
    /// Only completed Git reads live here.  An empty branch is a valid result
    /// (for a non-repository), not a stand-in for a failed worker.
    values: HashMap<String, Option<String>>,
    waiters: HashMap<BranchLabelRequest, Vec<BranchLabelWaiter>>,
    current: HashMap<usize, BranchLabelCurrent>,
    next_generation: u64,
}

struct BranchLabelWaiter {
    label: glib::WeakRef<gtk::Label>,
    label_key: usize,
    generation: u64,
}

/// A terminal branch read is tied to both its cwd and the label generation
/// that requested it. Leaving and returning to the same directory must not
/// join an earlier read that started before the intervening cwd change.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BranchLabelRequest {
    cwd: String,
    generation: u64,
}

struct BranchLabelCurrent {
    label: glib::WeakRef<gtk::Label>,
    cwd: String,
    generation: u64,
}

thread_local! {
    static BRANCH_LABEL_CACHE: RefCell<BranchLabelCache> = RefCell::new(BranchLabelCache::default());
}

pub(crate) fn registered_project_ssh_target(ssh_command: &[String]) -> Option<String> {
    restore::ssh_command_target(ssh_command)
}

use self::process::{
    build_shell_argv, connect_child_exit_cleanup, pane_spawn_argv, spawn_terminal_process,
    spawn_terminal_process_with_callback, PaneSpawnResultCallback, SpawnResultCallback,
};
#[cfg(test)]
use self::process::{child_exit_ui_action, ChildExitUiAction};
use self::signals::{
    connect_activity_termprop_protocol, connect_command_mark_tracking, connect_cwd_tracking,
    connect_notifications, connect_output_tracking, connect_pane_focus_tracking,
};
#[cfg(test)]
use self::signals::{is_taarof_activity_termprop, parse_termprop_activity_state};
use crate::config::GhosttyConfig;
use crate::pane::{
    PaneLeaf, PaneLocationState, PaneNavigationDirection, PaneNode, PaneProcessState, SplitChild,
};
use crate::probe::{ProbeSnapshot, ProbeTransition};
use crate::runtime::{RuntimeHandle, TerminalTabRegistration};
use crate::session::{RestoredPaneHint, RestoredTabLegend, SavedPaneNode};
use crate::sidebar::tab_root_widget_name;
use crate::{AppState, BroadcastScope, Tab};

const SELECTION_MODE_CSS_CLASS: &str = "selection-mode";
pub(crate) const MAX_CAPTURE_SCROLLBACK_LINES: u32 = 10_000;
const PCRE2_CASELESS: u32 = 0x00000008;
const PCRE2_MULTILINE: u32 = 0x00000400;
const LOCAL_SELECTION_DRAG_MIN_DISTANCE_PX: f64 = 3.0;
const LOCAL_SELECTION_OVERLAY_ALPHA: f64 = 0.58;

/// URL-ish regex pattern matching explicit HTTP(S) URLs and schemeless domains.
/// Matches:
///   - Standard domains: https://github.com/example/taarof
///   - Schemeless domains: github.com/example/taarof
///   - Localhost: http://localhost:3000
///   - IP addresses: http://192.168.1.1:8080
///   - With optional paths, query strings, and fragments
const URL_REGEX_PATTERN: &str = r"(?:https?://(?:[a-zA-Z0-9](?:[-a-zA-Z0-9]*[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[-a-zA-Z0-9]*[a-zA-Z0-9])?)*|(?:\d{1,3}\.){3}\d{1,3}|localhost)(?::\d{1,5})?(?:/[-a-zA-Z0-9+&@#/%?=~_|!:,.;()]*[-a-zA-Z0-9+&@#/%=~_|)])?|(?<![-A-Za-z0-9_.+/:])(?:(?:[a-zA-Z0-9](?:[-a-zA-Z0-9]*[a-zA-Z0-9])?\.)+[a-zA-Z][a-zA-Z0-9-]*(?::\d{1,5})?(?:/[-a-zA-Z0-9+&@#/%?=~_|!:,.;()]*[-a-zA-Z0-9+&@#/%=~_|])?|localhost:\d{1,5}))";
/// File-reference pattern: a path with an optional `:line[:col]` suffix. A
/// token must carry a path signal — either a slash (`src/lib/foo.ts`) or a
/// filename extension (`report.md`) — so hovering plain words does not paint a
/// clickable cursor. The `:line` suffix is no longer the false-positive guard;
/// the existence check in `resolve_local_file_match` is. A bare path resolves to
/// line 1, while `path:line[:col]` still resolves to the exact location.
const FILE_PATH_REGEX_PATTERN: &str = r"(?<![\w.+~/-])(?:~/(?:[\w.+-]+/)*[\w.+-]+|(?:\.{0,2}/)?(?:[\w.+-]+/)+[\w.+-]+|(?:\.{0,2}/)?[\w+-]+\.[A-Za-z][\w-]*)(?::\d+(?::\d+)?)?(?![\w+~/-])";
const ATTACH_SESSION_CONFIRM_DELAY: Duration = Duration::from_millis(500);

// Guards synchronous re-entrancy only. If VTE ever queues a `commit` signal
// through the GLib main loop instead of firing it inline within `feed_child`,
// the Drop reset will have cleared this flag and recursion will not be blocked.
thread_local! {
    static BROADCAST_FORWARDING_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Process-wide compiled link regexes. The PCRE2 compile is the expensive part
/// and is identical for every terminal, so it is done once per thread and
/// shared; only the per-terminal `match_add_regex` tag binding runs per pane.
struct TerminalLinkRegexes {
    url: Option<vte::Regex>,
    file_path: Option<vte::Regex>,
}

thread_local! {
    static TERMINAL_LINK_REGEXES: std::cell::OnceCell<TerminalLinkRegexes> =
        const { std::cell::OnceCell::new() };
    #[cfg(test)]
    static TERMINAL_LINK_REGEX_COMPILES: Cell<u32> = const { Cell::new(0) };
}

fn compile_terminal_link_regex(kind: &str, pattern: &str) -> Option<vte::Regex> {
    match vte::Regex::for_match(pattern, terminal_url_regex_compile_flags()) {
        Ok(rx) => Some(rx),
        Err(e) => {
            eprintln!("taarof: failed to compile {kind} regex: {e}");
            None
        }
    }
}

/// Run `f` with the thread's shared link regexes, compiling them on first use.
fn with_terminal_link_regexes<R>(f: impl FnOnce(&TerminalLinkRegexes) -> R) -> R {
    TERMINAL_LINK_REGEXES.with(|cell| {
        let regexes = cell.get_or_init(|| {
            #[cfg(test)]
            TERMINAL_LINK_REGEX_COMPILES.with(|c| c.set(c.get() + 1));
            TerminalLinkRegexes {
                url: compile_terminal_link_regex("URL", URL_REGEX_PATTERN),
                file_path: compile_terminal_link_regex("file-path", FILE_PATH_REGEX_PATTERN),
            }
        });
        f(regexes)
    })
}

type RestoredPaneSpawn = (u32, vte::Terminal, Option<String>, Option<Vec<String>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalLinkKind {
    PlainUrl,
    Hyperlink,
}

/// Apply Ghostty config to a VTE terminal: colors, font, cursor, clipboard, padding.
fn configure_terminal(terminal: &vte::Terminal, cfg: &GhosttyConfig) {
    // Colors — from Ghostty theme (Omarchy or whatever is configured)
    let bg = gdk::RGBA::parse(&cfg.background)
        .unwrap_or_else(|_| gdk::RGBA::parse("#14110c").expect("default background is valid"));
    let fg = gdk::RGBA::parse(&cfg.foreground)
        .unwrap_or_else(|_| gdk::RGBA::parse("#efe7d6").expect("default foreground is valid"));
    let palette: Vec<gdk::RGBA> = cfg
        .palette
        .iter()
        .map(|c| {
            gdk::RGBA::parse(c).unwrap_or_else(|_| {
                gdk::RGBA::parse("#34291a").expect("default palette color is valid")
            })
        })
        .collect();
    let palette_refs: Vec<&gdk::RGBA> = palette.iter().collect();
    terminal.set_colors(Some(&fg), Some(&bg), &palette_refs);

    // Cursor color
    if let Ok(cursor_rgba) = gdk::RGBA::parse(&cfg.cursor_color) {
        terminal.set_color_cursor(Some(&cursor_rgba));
    }

    // Selection colors
    if let Ok(sel_fg) = gdk::RGBA::parse(&cfg.selection_fg) {
        terminal.set_color_highlight_foreground(Some(&sel_fg));
    }
    if let Ok(sel_bg) = gdk::RGBA::parse(&cfg.selection_bg) {
        terminal.set_color_highlight(Some(&sel_bg));
    }

    // Font
    let font_desc = gtk::pango::FontDescription::from_string(&cfg.font_description());
    terminal.set_font(Some(&font_desc));

    // Cursor style
    match cfg.cursor_style.as_str() {
        "bar" | "ibeam" => terminal.set_cursor_shape(vte::CursorShape::Ibeam),
        "underline" => terminal.set_cursor_shape(vte::CursorShape::Underline),
        _ => terminal.set_cursor_shape(vte::CursorShape::Block),
    }
    if cfg.cursor_blink {
        terminal.set_cursor_blink_mode(vte::CursorBlinkMode::On);
    } else {
        terminal.set_cursor_blink_mode(vte::CursorBlinkMode::Off);
    }

    // Copy-on-select: when text is selected, auto-copy to clipboard
    if cfg.copy_on_select {
        terminal.connect_selection_changed(|term| {
            if term.has_selection() {
                self::broadcast::copy_vte_selection_and_record(term);
            }
        });
    }

    // Scrollback
    terminal.set_scrollback_lines(cfg.scrollback_lines);

    // Allow clickable hyperlinks (OSC 8)
    terminal.set_allow_hyperlink(true);

    // Add regex matching for plain URLs (http/https)
    setup_url_regex_matching(terminal);

    // F006: Disable audible bell (we handle it visually), enable OSC 777
    terminal.set_audible_bell(false);
    terminal.set_enable_legacy_osc777(true);

    // Padding — applied as CSS margin on the terminal widget
    if cfg.padding_x > 0 || cfg.padding_y > 0 {
        terminal.set_margin_start(cfg.padding_x);
        terminal.set_margin_end(cfg.padding_x);
        terminal.set_margin_top(cfg.padding_y);
        terminal.set_margin_bottom(cfg.padding_y);
    }
}

/// Set up regex matching for clickable URLs in terminal output.
/// This uses VTE's match_add_regex to detect http/https URLs.
fn setup_url_regex_matching(terminal: &vte::Terminal) {
    // Reuse the process-wide compiled regexes; only match_add_regex (which
    // binds a per-terminal tag) runs per pane. The URL regex is mandatory —
    // bail if it never compiled — while the file-path regex is optional.
    let Some((tag, file_tag)) = with_terminal_link_regexes(|r| {
        let url = r.url.as_ref()?;
        // Add the regexes and keep their tags so the motion controller below
        // can choose a cursor from the resolved target, not just token shape.
        let tag = terminal.match_add_regex(url, 0);

        // File references: bare paths and path:line[:col] -> peek/open. Do not
        // use match_set_cursor_name here: VTE would apply it to every
        // path-shaped token before taarof can verify that the local file exists.
        let file_tag = r
            .file_path
            .as_ref()
            .map(|rx| terminal.match_add_regex(rx, 0));
        Some((tag, file_tag))
    }) else {
        return;
    };

    // Add a click handler to detect URL clicks
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);

    let term_for_click = terminal.clone();
    click.connect_pressed(move |gesture, n_press, x, y| {
        // Only handle single clicks
        if n_press != 1 {
            return;
        }

        // Check if Ctrl is held (standard modifier for opening URLs)
        let current_event = gesture.current_event();
        let mods = current_event
            .as_ref()
            .map(|e| e.modifier_state())
            .unwrap_or(gdk::ModifierType::empty());

        // Require Ctrl+Click to open URLs (prevents accidental opens)
        if !mods.contains(gdk::ModifierType::CONTROL_MASK) {
            return;
        }

        // OSC 8 hyperlinks first (explicit links).
        if let Some(uri) = term_for_click.check_hyperlink_at(x, y) {
            if show_terminal_hyperlink_confirmation_popover(&term_for_click, &uri, x, y) {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
            return;
        }

        // Then regex matches, dispatched by tag.
        let (match_text, matched_tag) = term_for_click.check_match_at(x, y);
        if let Some(text) = match_text {
            if (matched_tag == tag || Some(matched_tag) == file_tag)
                && launch_terminal_link_match(&term_for_click, &text, x, y)
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        }
    });

    terminal.add_controller(click);

    // VTE's per-regex cursor API cannot make the cursor conditional on file
    // existence. Resolve file matches on motion instead, while retaining the
    // established cursor for plain URL matches and OSC 8 hyperlinks. Cache the
    // last link token/location while the pointer remains over it so normal
    // motion within a token does not repeat filesystem metadata calls.
    let motion = gtk::EventControllerMotion::new();
    let term_for_motion = terminal.clone();
    let last_link_hover = Rc::new(RefCell::new(
        None::<(String, Option<String>, Option<String>, Option<&'static str>)>,
    ));
    let cursor_update_generation = Rc::new(Cell::new(0_u64));
    {
        let last_link_hover = last_link_hover.clone();
        let cursor_update_generation = cursor_update_generation.clone();
        motion.connect_motion(move |_, x, y| {
            let cursor_name = if let Some(uri) = term_for_motion.check_hyperlink_at(x, y) {
                last_link_hover.borrow_mut().take();
                terminal_hyperlink_hover_cursor_name(&uri)
            } else {
                let (match_text, matched_tag) = term_for_motion.check_match_at(x, y);
                match match_text {
                    Some(text) if matched_tag == tag || Some(matched_tag) == file_tag => {
                        let (cwd, cwd_host) = terminal_location_metadata(&term_for_motion);
                        let cached_cursor = last_link_hover.borrow().as_ref().and_then(
                            |(cached_text, cached_cwd, cached_host, cursor)| {
                                (cached_text == text.as_str()
                                    && cached_cwd == &cwd
                                    && cached_host == &cwd_host)
                                    .then_some(*cursor)
                            },
                        );
                        cached_cursor.unwrap_or_else(|| {
                            let cursor = terminal_match_hover_cursor_name(
                                &text,
                                cwd.as_deref(),
                                cwd_host.as_deref(),
                            );
                            *last_link_hover.borrow_mut() =
                                Some((text.to_string(), cwd, cwd_host, cursor));
                            cursor
                        })
                    }
                    _ => {
                        last_link_hover.borrow_mut().take();
                        None
                    }
                }
            };
            // VTE also updates its cursor during motion dispatch. Apply the
            // existence-aware result once that dispatch finishes so VTE cannot
            // immediately overwrite it with the terminal's text cursor. The
            // generation guard coalesces rapid motion and drops stale idles.
            let generation = cursor_update_generation.get().wrapping_add(1);
            cursor_update_generation.set(generation);
            let term_for_cursor = term_for_motion.clone();
            let cursor_update_generation = cursor_update_generation.clone();
            glib::idle_add_local_once(move || {
                if cursor_update_generation.get() == generation {
                    term_for_cursor.set_cursor_from_name(cursor_name);
                }
            });
        });
    }
    let term_for_leave = terminal.clone();
    motion.connect_leave(move |_| {
        cursor_update_generation.set(cursor_update_generation.get().wrapping_add(1));
        last_link_hover.borrow_mut().take();
        term_for_leave.set_cursor_from_name(None);
    });
    terminal.add_controller(motion);
}

fn terminal_url_regex_compile_flags() -> u32 {
    PCRE2_CASELESS | PCRE2_MULTILINE
}

fn launchable_uri_for_terminal_link(link: &str, kind: TerminalLinkKind) -> Option<String> {
    let link = link.trim();
    if link.is_empty() {
        return None;
    }

    let scheme = glib::Uri::parse_scheme(link)?.to_ascii_lowercase();
    match kind {
        TerminalLinkKind::PlainUrl => {
            matches!(scheme.as_str(), "http" | "https").then(|| link.to_string())
        }
        TerminalLinkKind::Hyperlink => match scheme.as_str() {
            "javascript" | "data" => None,
            _ => glib::Uri::parse(link, glib::UriFlags::NONE)
                .ok()
                .map(|_| link.to_string()),
        },
    }
}

fn terminal_hyperlink_confirmation_item(link: &str) -> Option<TerminalLinkDisambiguationItem> {
    let uri = launchable_uri_for_terminal_link(link, TerminalLinkKind::Hyperlink)?;
    Some(TerminalLinkDisambiguationItem::Hyperlink {
        label: escape_menu_label_underscores(&format!("Open {uri}")),
        uri,
    })
}

/// A clickable file reference parsed from terminal text: `path:line[:col]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileMatch {
    pub path: String,
    pub line: u32,
    pub col: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum TerminalLinkCandidate {
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        col: Option<u32>,
    },
    Url {
        url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TerminalLinkCandidates {
    pub ambiguous: bool,
    pub candidates: Vec<TerminalLinkCandidate>,
}

#[derive(Debug, serde::Deserialize)]
struct TerminalLinkSharedConfig {
    ambiguous_file_tlds: Vec<String>,
    url_only_tlds: Vec<String>,
    non_tld_file_extensions: Vec<String>,
}

fn terminal_link_shared_config() -> &'static TerminalLinkSharedConfig {
    static CONFIG: OnceLock<TerminalLinkSharedConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        serde_json::from_str(include_str!("../../fixtures/terminal-links.json"))
            .expect("terminal link fixture config should parse")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedTerminalLink {
    File {
        path: std::path::PathBuf,
        line: u32,
        col: Option<u32>,
    },
    Url(String),
    NeedsDisambiguation {
        candidates: TerminalLinkCandidates,
        existing: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalLinkDisambiguationItem {
    File {
        label: String,
        path: std::path::PathBuf,
        line: u32,
        col: Option<u32>,
    },
    Url {
        label: String,
        url: String,
    },
    Hyperlink {
        label: String,
        uri: String,
    },
}

#[derive(Debug, Default)]
struct TerminalLinkActivationGuard {
    claimed: Cell<bool>,
}

impl TerminalLinkActivationGuard {
    fn claim(&self) -> bool {
        !self.claimed.replace(true)
    }
}

fn escape_menu_label_underscores(label: &str) -> String {
    label.replace('_', "__")
}

/// Parse a `path:line` or `path:line:col` reference. Returns `None` unless a
/// numeric line is present, which is the primary false-positive guard.
pub(crate) fn parse_path_line_col(text: &str) -> Option<FileMatch> {
    let text = text.trim();
    // rsplitn yields the rightmost fields first.
    let parts: Vec<&str> = text.rsplitn(3, ':').collect();
    match parts.as_slice() {
        [col, line, path] => {
            let col = col.parse::<u32>().ok()?;
            let line = line.parse::<u32>().ok()?;
            if path.is_empty() {
                return None;
            }
            Some(FileMatch {
                path: (*path).to_string(),
                line,
                col: Some(col),
            })
        }
        [line, path] => {
            let line = line.parse::<u32>().ok()?;
            if path.is_empty() {
                return None;
            }
            Some(FileMatch {
                path: (*path).to_string(),
                line,
                col: None,
            })
        }
        _ => None,
    }
}

/// Resolve a parsed file reference to an absolute local path candidate.
/// Returns `None` for remote panes (a local editor cannot open them) and for
/// relative paths with no known working directory. Does NOT check existence —
/// the caller verifies the file is present before launching.
/// Expand a leading `~` or `~/` to `home`. Other forms (`~user`, an embedded
/// `~`, already-absolute or plain relative paths) are returned unchanged.
/// `std::path` never expands `~` itself, so a cwd taken from a shell *title*
/// — which renders `$HOME` as `~` — must be expanded before it can be joined
/// and existence-checked. Only reached for local panes (callers bail on a
/// remote `cwd_host`), so expanding against the local `$HOME` is correct.
fn expand_leading_tilde(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home.to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", home.trim_end_matches('/'), rest),
        None => path.to_string(),
    }
}

/// The local `$HOME`, used to expand a tilde-form cwd/path during file
/// resolution. Empty when unset, which makes `expand_leading_tilde` a no-op.
fn local_home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

pub(crate) fn resolve_file_path(
    m: &FileMatch,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<std::path::PathBuf> {
    if cwd_host.is_some() {
        return None;
    }
    let home = local_home();
    let expanded = expand_leading_tilde(&m.path, &home);
    let path = std::path::Path::new(&expanded);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        let cwd = expand_leading_tilde(cwd?, &home);
        Some(std::path::Path::new(&cwd).join(path))
    }
}

/// True when the final path component carries a filename extension whose shape
/// signals a real file: a non-empty stem, a non-empty extension that is all
/// ASCII alphanumeric and contains at least one letter. `report.md`, `foo.ts`,
/// and `example.com` pass; `1.5` and `192.168` (numeric-only extension) fail.
fn filename_has_extension(token: &str) -> bool {
    let name = token.rsplit('/').next().unwrap_or(token);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || ext.is_empty() {
        return false;
    }
    ext.chars().all(|c| c.is_ascii_alphanumeric()) && ext.chars().any(|c| c.is_ascii_alphabetic())
}

fn filename_extension(token: &str) -> Option<String> {
    let path_part = parse_path_line_col(token)
        .map(|m| m.path)
        .unwrap_or_else(|| token.to_string());
    let name = path_part.rsplit('/').next().unwrap_or(&path_part);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty()
        || ext.is_empty()
        || !ext.chars().all(|c| c.is_ascii_alphanumeric())
        || !ext.chars().any(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn explicit_file_prefix(token: &str) -> bool {
    token.starts_with("./") || token.starts_with("../") || token.starts_with('/')
}

fn explicit_http_scheme(token: &str) -> bool {
    token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || token
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn is_ambiguous_file_tld(tld: &str) -> bool {
    terminal_link_shared_config()
        .ambiguous_file_tlds
        .iter()
        .any(|known| known == tld)
}

fn is_url_only_tld(tld: &str) -> bool {
    terminal_link_shared_config()
        .url_only_tlds
        .iter()
        .any(|known| known == tld)
}

fn is_non_tld_file_extension(ext: &str) -> bool {
    terminal_link_shared_config()
        .non_tld_file_extensions
        .iter()
        .any(|known| known == ext)
}

fn is_known_url_tld(tld: &str) -> bool {
    !is_non_tld_file_extension(tld) && (is_ambiguous_file_tld(tld) || is_url_only_tld(tld))
}

fn url_candidate_uses_ambiguous_file_tld(url: &str) -> bool {
    let Some(authority_and_path) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = authority_and_path
        .split_once('/')
        .map_or(authority_and_path, |(authority, _)| authority);
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _port)| host);
    host.rsplit('.')
        .next()
        .is_some_and(|tld| is_ambiguous_file_tld(&tld.to_ascii_lowercase()))
}

fn has_ambiguous_tld_url_candidate(candidates: &[TerminalLinkCandidate]) -> bool {
    candidates.iter().any(|candidate| match candidate {
        TerminalLinkCandidate::Url { url } => url_candidate_uses_ambiguous_file_tld(url),
        TerminalLinkCandidate::File { .. } => false,
    })
}

fn has_exactly_one_file_and_one_url_candidate(candidates: &[TerminalLinkCandidate]) -> bool {
    let file_count = candidates
        .iter()
        .filter(|candidate| matches!(candidate, TerminalLinkCandidate::File { .. }))
        .count();
    let url_count = candidates
        .iter()
        .filter(|candidate| matches!(candidate, TerminalLinkCandidate::Url { .. }))
        .count();
    file_count == 1 && url_count == 1
}

fn trim_terminal_link_token_punctuation(text: &str) -> &str {
    const LEADING: &[char] = &['(', '[', '{', '<', '\'', '"'];
    const TRAILING: &[char] = &['.', ',', ':', ';', '!', '?', ']', '}', '>', '\'', '"'];
    let mut token = text.trim().trim_start_matches(LEADING);
    loop {
        let Some(last) = token.chars().next_back() else {
            return token;
        };
        if TRAILING.contains(&last) || (last == ')' && !token.contains('(')) {
            token = &token[..token.len() - last.len_utf8()];
        } else {
            return token;
        }
    }
}

fn valid_domain_label(label: &str) -> bool {
    if label.is_empty()
        || label.starts_with('-')
        || label.ends_with('-')
        || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return false;
    }
    true
}

fn valid_domain_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2 && labels.iter().all(|label| valid_domain_label(label))
}

fn split_schemeless_authority(token: &str) -> Option<(&str, Option<u16>)> {
    let authority = token
        .split_once('/')
        .map_or(token, |(authority, _)| authority);
    if authority.is_empty() {
        return None;
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let port = port.parse::<u32>().ok()?;
        if port > u16::MAX as u32 {
            return None;
        }
        Some((host, Some(port as u16)))
    } else {
        Some((authority, None))
    }
}

fn schemeless_url_candidate(token: &str) -> Option<TerminalLinkCandidate> {
    let (host, port) = split_schemeless_authority(token)?;
    let scheme = if host.eq_ignore_ascii_case("localhost") {
        port?;
        "http"
    } else {
        if !valid_domain_host(host) {
            return None;
        }
        let tld = host.rsplit('.').next()?.to_ascii_lowercase();
        if !is_known_url_tld(&tld) {
            return None;
        }
        "https"
    };

    Some(TerminalLinkCandidate::Url {
        url: format!("{scheme}://{token}"),
    })
}

fn file_candidate(token: &str) -> Option<TerminalLinkCandidate> {
    if let Some(parsed) = parse_path_line_col(token) {
        if explicit_file_prefix(&parsed.path)
            || parsed.path.contains('/')
            || filename_extension(&parsed.path)
                .is_some_and(|ext| !is_url_only_tld(&ext) || is_ambiguous_file_tld(&ext))
        {
            return Some(TerminalLinkCandidate::File {
                path: parsed.path,
                line: Some(parsed.line),
                col: parsed.col,
            });
        }
        return None;
    }

    if explicit_file_prefix(token)
        || token.contains('/')
        || filename_extension(token)
            .is_some_and(|ext| !is_url_only_tld(&ext) || is_ambiguous_file_tld(&ext))
    {
        return Some(TerminalLinkCandidate::File {
            path: token.to_string(),
            line: None,
            col: None,
        });
    }
    None
}

pub(crate) fn match_terminal_link_candidates(text: &str) -> TerminalLinkCandidates {
    let token = trim_terminal_link_token_punctuation(text);
    if token.is_empty() || token.contains(char::is_whitespace) {
        return TerminalLinkCandidates {
            ambiguous: false,
            candidates: Vec::new(),
        };
    }

    if explicit_http_scheme(token) {
        return TerminalLinkCandidates {
            ambiguous: false,
            candidates: vec![TerminalLinkCandidate::Url {
                url: token.to_string(),
            }],
        };
    }

    if explicit_file_prefix(token) {
        return TerminalLinkCandidates {
            ambiguous: false,
            candidates: file_candidate(token).into_iter().collect(),
        };
    }

    let url = schemeless_url_candidate(token);
    let file = file_candidate(token);
    let candidates = match (url, file) {
        (Some(url), Some(file)) if token.contains('/') => vec![url, file],
        (Some(url), Some(file)) => vec![file, url],
        (Some(url), None) => vec![url],
        (None, Some(file)) => vec![file],
        (None, None) => Vec::new(),
    };

    let ambiguous = candidates.len() > 1;
    debug_assert!(!ambiguous || has_exactly_one_file_and_one_url_candidate(&candidates));
    TerminalLinkCandidates {
        ambiguous,
        candidates,
    }
}

/// Shape check for a bare path token. Rejects empty/whitespace-bearing tokens,
/// URLs (`://`), and the `.`/`..` directory shorthands, then requires a path
/// signal: a slash or a filename extension. This only screens obvious non-paths
/// so hover noise stays low — existence is the real gate on the open path.
fn looks_like_path_candidate(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.contains(char::is_whitespace) || t.contains("://") || t == "." || t == ".."
    {
        return false;
    }
    t.contains('/') || filename_has_extension(t)
}

/// Strip a small set of surrounding punctuation from a path token, mirroring the
/// URL trimming in the web client so `(src/main.rs)` or `report.md.` resolve.
/// Leading `.`/`/` are intentionally kept so `../foo` and `./bar` survive.
fn trim_path_token_punctuation(text: &str) -> &str {
    const LEADING: &[char] = &['(', '[', '{', '\'', '"'];
    const TRAILING: &[char] = &['.', ',', ':', ';', '!', '?', ')', ']', '}', '\'', '"'];
    text.trim_start_matches(LEADING).trim_end_matches(TRAILING)
}

/// Resolve a bare path token (no `:line` required) against the pane cwd, gated
/// solely on the target being a regular file. Returns `None` for remote panes and for tokens
/// that do not look like a path. This is the loose-matching entry point EXAMPLE-94
/// relies on: a lookalike that does not exist yields no openable target.
fn resolve_existing_bare_path(
    text: &str,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<std::path::PathBuf> {
    if cwd_host.is_some() {
        return None;
    }
    let token = trim_path_token_punctuation(text.trim());
    if !looks_like_path_candidate(token) {
        return None;
    }
    let home = local_home();
    let token = expand_leading_tilde(token, &home);
    let path = std::path::Path::new(&token);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = expand_leading_tilde(cwd?, &home);
        std::path::Path::new(&cwd).join(path)
    };
    resolved.is_file().then_some(resolved)
}

fn resolved_file_candidate(
    candidate: &TerminalLinkCandidate,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<(std::path::PathBuf, u32, Option<u32>)> {
    let TerminalLinkCandidate::File { path, line, col } = candidate else {
        return None;
    };
    let line = line.unwrap_or(1);
    let parsed = FileMatch {
        path: path.clone(),
        line,
        col: *col,
    };
    let resolved = resolve_file_path(&parsed, cwd, cwd_host)?;
    resolved.is_file().then_some((resolved, line, *col))
}

fn resolve_terminal_link_candidates(
    candidates: &TerminalLinkCandidates,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<ResolvedTerminalLink> {
    if candidates.candidates.is_empty() {
        return None;
    }

    let can_stat_local_files = cwd_host.is_none();
    let mut ordered = candidates.candidates.clone();
    let mut existing_file = None;
    let mut existing_path = None;

    if can_stat_local_files {
        for (index, candidate) in candidates.candidates.iter().enumerate() {
            if let Some((path, line, col)) = resolved_file_candidate(candidate, cwd, None) {
                existing_path = Some(path.clone());
                existing_file = Some((path, line, col));
                if index != 0 {
                    let candidate = ordered.remove(index);
                    ordered.insert(0, candidate);
                }
                break;
            }
        }
    }

    let should_disambiguate_existing = candidates.ambiguous && existing_path.is_some();
    let should_report_unresolved_remote = candidates.ambiguous
        && !can_stat_local_files
        && matches!(
            candidates.candidates.first(),
            Some(TerminalLinkCandidate::File { .. })
        );
    if should_disambiguate_existing || should_report_unresolved_remote {
        return Some(ResolvedTerminalLink::NeedsDisambiguation {
            candidates: TerminalLinkCandidates {
                ambiguous: true,
                candidates: ordered,
            },
            existing: existing_path,
        });
    }

    if let Some((path, line, col)) = existing_file {
        return Some(ResolvedTerminalLink::File { path, line, col });
    }

    if candidates.ambiguous
        && can_stat_local_files
        && matches!(
            candidates.candidates.first(),
            Some(TerminalLinkCandidate::File { .. })
        )
        && has_ambiguous_tld_url_candidate(&candidates.candidates)
    {
        // Suppress only file-first ambiguity: no slash, and a URL TLD collides
        // with a filename extension. Without an existing local file, opening the
        // URL would let untrusted terminal output trigger navigation to domains
        // an attacker can control. #272 can replace this no-op with a popover.
        return None;
    }

    ordered.iter().find_map(|candidate| match candidate {
        TerminalLinkCandidate::Url { url } => Some(ResolvedTerminalLink::Url(url.clone())),
        TerminalLinkCandidate::File { .. } => None,
    })
}

fn terminal_link_resolution_with_metadata(
    match_text: &str,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<ResolvedTerminalLink> {
    let candidates = match_terminal_link_candidates(match_text);
    let cwd_host = cwd_host.filter(|_| is_remote_host(cwd_host));
    resolve_terminal_link_candidates(&candidates, cwd, cwd_host)
}

fn terminal_link_resolution(
    terminal: &vte::Terminal,
    match_text: &str,
) -> Option<ResolvedTerminalLink> {
    let (cwd, cwd_host) = terminal_location_metadata(terminal);
    terminal_link_resolution_with_metadata(match_text, cwd.as_deref(), cwd_host.as_deref())
}

/// Extract the whitespace-delimited token that contains char index `col` within
/// `row`. Returns `None` when `col` is out of range or lands on whitespace.
/// Indexing is by char, not terminal cell, so wide (CJK/emoji) glyphs can shift
/// the boundary; paths are ASCII in practice so this is fine.
fn token_at_column(row: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = row.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let start = chars[..col]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |i| i + 1);
    let end = chars[col..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |i| col + i);
    let token: String = chars[start..end].iter().collect();
    (!token.is_empty()).then_some(token)
}

/// Capture the whitespace-delimited token under the terminal's text cursor. Used
/// by the keyboard peek path so a path can be opened with no selection.
fn token_under_cursor(terminal: &vte::Terminal) -> Option<String> {
    let (col, row) = terminal.cursor_position();
    let cols = terminal.column_count();
    if cols <= 0 || col < 0 {
        return None;
    }
    let (text, _len) = terminal.text_range_format(vte::Format::Text, row, 0, row, cols - 1);
    let text = text.map(|g| g.to_string())?;
    token_at_column(&text, col as usize)
}

/// Turn an `open_command` template into an argv vector. The template is split
/// into tokens FIRST, then `{path}`/`{line}`/`{col}` are substituted into
/// individual tokens, so a path containing spaces or shell metacharacters always
/// stays exactly one argument. The result is passed to `Command` directly — it
/// is never handed to a shell. When `col` is absent, a trailing `:{col}` is
/// dropped so the editor receives `path:line`, not `path:line:`.
pub(crate) fn build_editor_argv(
    open_command: &str,
    path: &str,
    line: u32,
    col: Option<u32>,
) -> Option<Vec<String>> {
    let tokens: Vec<&str> = open_command.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let line_s = line.to_string();
    let col_s = col.map(|c| c.to_string()).unwrap_or_default();
    let argv = tokens
        .into_iter()
        .map(|token| {
            let token = if col.is_none() {
                token.strip_suffix(":{col}").unwrap_or(token)
            } else {
                token
            };
            token
                .replace("{path}", path)
                .replace("{line}", &line_s)
                .replace("{col}", &col_s)
        })
        .collect();
    Some(argv)
}

/// Open a clickable terminal link with the system's default URI handler.
/// Plain-text matches are limited to HTTP(S) URLs. OSC 8 hyperlinks can use
/// other URI schemes, but explicitly dangerous schemes like javascript: and
/// data: are rejected.
fn open_terminal_link(link: &str, kind: TerminalLinkKind) {
    let Some(uri) = launchable_uri_for_terminal_link(link, kind) else {
        eprintln!("taarof: refusing to open unsupported terminal link: {link}");
        return;
    };

    crate::browser::open_url(&uri);
}

/// Activate an OSC-8 target after the destination-confirmation action has been
/// selected. Existing file hyperlinks preserve their editor/peek behavior;
/// all other accepted schemes use the system URI handler.
fn activate_terminal_hyperlink(terminal: &vte::Terminal, uri: &str) {
    if let Some(rest) = uri.strip_prefix("file://") {
        let path_part = rest.split_once('/').map(|(_, path)| format!("/{path}"));
        if let Some(path) = path_part {
            if launch_file_match(terminal, &path) {
                return;
            }
        }
    }
    open_terminal_link(uri, TerminalLinkKind::Hyperlink);
}

/// Spawn the configured editor for a resolved local file. Failures are logged,
/// not surfaced as modal errors (matching `open_terminal_link`).
fn launch_editor(path: &std::path::Path, line: u32, col: Option<u32>) {
    let open_command = crate::config::editor_config().open_command;
    let Some(argv) = build_editor_argv(&open_command, &path.to_string_lossy(), line, col) else {
        eprintln!("taarof: invalid editor open_command: {open_command:?}");
        return;
    };
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    if let Err(e) = crate::child_process::spawn_and_reap(&mut command) {
        eprintln!("taarof: failed to launch editor {program:?}: {e}");
    }
}

/// Launch the configured editor for an existing local file at line 1. Used by
/// callers that only have a bare path (e.g. the recent-files popup's eject).
pub(crate) fn open_path_in_editor(path: &std::path::Path) {
    open_path_in_editor_at(path, 1, None);
}

/// Launch the configured editor for an existing local file at a specific
/// location. No-ops when the path is not a regular file. Used by the peek overlay's
/// "eject to editor" affordance, which knows the line the user was viewing.
pub(crate) fn open_path_in_editor_at(path: &std::path::Path, line: u32, col: Option<u32>) {
    if !path.is_file() {
        return;
    }
    launch_editor(path, line, col);
}

/// Resolve a matched file token against pane location metadata. Keeping this
/// independent of VTE lets click handling and hover affordance share the exact
/// same existence and local-host gate.
fn resolve_local_file_match_with_metadata(
    match_text: &str,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<(std::path::PathBuf, u32, Option<u32>)> {
    // A title-derived host is not local-host-filtered the way an OSC 7 host is
    // (see parse_osc7_uri), so normalize here: a pane on the local machine must
    // not be treated as remote just because its title carries the hostname.
    let cwd_host = cwd_host.filter(|_| is_remote_host(cwd_host));

    // Exact `path:line[:col]` reference: keep opening at the precise location.
    if let Some(parsed) = parse_path_line_col(match_text) {
        if let Some(resolved) = resolve_file_path(&parsed, cwd, cwd_host) {
            if resolved.is_file() {
                return Some((resolved, parsed.line, parsed.col));
            }
        }
    }

    // Otherwise fall back to a bare path that exists, opened at the top. A stale
    // `path:line` whose file is missing lands here and is rejected by existence.
    let resolved = resolve_existing_bare_path(match_text, cwd, cwd_host)?;
    Some((resolved, 1, None))
}

fn terminal_match_hover_cursor_name(
    match_text: &str,
    cwd: Option<&str>,
    cwd_host: Option<&str>,
) -> Option<&'static str> {
    match terminal_link_resolution_with_metadata(match_text, cwd, cwd_host) {
        Some(ResolvedTerminalLink::File { .. }) => Some("pointer"),
        Some(ResolvedTerminalLink::Url(_))
        | Some(ResolvedTerminalLink::NeedsDisambiguation { .. }) => Some("hand2"),
        None => None,
    }
}

fn terminal_hyperlink_hover_cursor_name(uri: &str) -> Option<&'static str> {
    launchable_uri_for_terminal_link(uri, TerminalLinkKind::Hyperlink).map(|_| "hand2")
}

/// Resolve a matched `path:line[:col]` against the terminal's CWD to an existing
/// local file. Returns the resolved path and its parsed location, or `None` when
/// the reference is not a local file that exists.
fn resolve_local_file_match(
    terminal: &vte::Terminal,
    match_text: &str,
) -> Option<(std::path::PathBuf, u32, Option<u32>)> {
    let (cwd, cwd_host) = terminal_location_metadata(terminal);
    resolve_local_file_match_with_metadata(match_text, cwd.as_deref(), cwd_host.as_deref())
}

/// Resolve a matched `path:line[:col]` against the terminal's CWD and open it
/// according to the configured `[editor] click_action` (peek overlay by default,
/// or eject to the external editor). Returns `true` when handled.
fn launch_file_match(terminal: &vte::Terminal, match_text: &str) -> bool {
    let Some((resolved, line, col)) = resolve_local_file_match(terminal, match_text) else {
        return false;
    };
    match crate::config::editor_config().click_action {
        crate::config::ClickAction::Peek => crate::peek::peek_path(&resolved, Some(line)),
        crate::config::ClickAction::Editor => launch_editor(&resolved, line, col),
    }
    true
}

fn launch_terminal_link_match(terminal: &vte::Terminal, match_text: &str, x: f64, y: f64) -> bool {
    match terminal_link_resolution(terminal, match_text) {
        Some(ResolvedTerminalLink::File { path, line, col }) => {
            match crate::config::editor_config().click_action {
                crate::config::ClickAction::Peek => crate::peek::peek_path(&path, Some(line)),
                crate::config::ClickAction::Editor => launch_editor(&path, line, col),
            }
            true
        }
        Some(ResolvedTerminalLink::Url(url)) => {
            open_terminal_link(&url, TerminalLinkKind::PlainUrl);
            true
        }
        Some(ResolvedTerminalLink::NeedsDisambiguation {
            candidates,
            existing,
        }) => show_terminal_link_disambiguation_popover(terminal, candidates, existing, x, y),
        None => false,
    }
}

fn terminal_link_disambiguation_items(
    candidates: &TerminalLinkCandidates,
    existing: Option<&std::path::Path>,
) -> Vec<TerminalLinkDisambiguationItem> {
    candidates
        .candidates
        .iter()
        .filter_map(|candidate| match candidate {
            TerminalLinkCandidate::File { line, col, .. } => {
                existing.map(|existing_path| TerminalLinkDisambiguationItem::File {
                    label: escape_menu_label_underscores(&format!(
                        "Open {}",
                        existing_path.display()
                    )),
                    path: existing_path.to_path_buf(),
                    line: line.unwrap_or(1),
                    col: *col,
                })
            }
            TerminalLinkCandidate::Url { url } => Some(TerminalLinkDisambiguationItem::Url {
                label: escape_menu_label_underscores(&format!("Open {url}")),
                url: url.clone(),
            }),
        })
        .collect()
}

fn show_terminal_link_disambiguation_popover(
    terminal: &vte::Terminal,
    candidates: TerminalLinkCandidates,
    existing: Option<std::path::PathBuf>,
    x: f64,
    y: f64,
) -> bool {
    let items = terminal_link_disambiguation_items(&candidates, existing.as_deref());
    show_terminal_link_items_popover(terminal, items, x, y)
}

fn show_terminal_hyperlink_confirmation_popover(
    terminal: &vte::Terminal,
    uri: &str,
    x: f64,
    y: f64,
) -> bool {
    let Some(item) = terminal_hyperlink_confirmation_item(uri) else {
        eprintln!("taarof: refusing to open unsupported terminal link: {uri}");
        return false;
    };
    show_terminal_link_items_popover(terminal, vec![item], x, y)
}

fn show_terminal_link_items_popover(
    terminal: &vte::Terminal,
    items: Vec<TerminalLinkDisambiguationItem>,
    x: f64,
    y: f64,
) -> bool {
    if items.is_empty() {
        return false;
    }

    let menu = gio::Menu::new();
    for (index, item) in items.iter().enumerate() {
        menu.append(Some(item.label()), Some(&format!("link.open-{index}")));
    }

    let group = gio::SimpleActionGroup::new();
    for (index, item) in items.into_iter().enumerate() {
        let action = gio::SimpleAction::new(&format!("open-{index}"), None);
        let terminal_for_action = terminal.clone();
        let activation_guard = TerminalLinkActivationGuard::default();
        action.connect_activate(move |action, _| {
            if !activation_guard.claim() {
                return;
            }
            action.set_enabled(false);
            match &item {
                TerminalLinkDisambiguationItem::File {
                    path, line, col, ..
                } => match crate::config::editor_config().click_action {
                    crate::config::ClickAction::Peek => crate::peek::peek_path(path, Some(*line)),
                    crate::config::ClickAction::Editor => launch_editor(path, *line, *col),
                },
                TerminalLinkDisambiguationItem::Url { url, .. } => {
                    open_terminal_link(url, TerminalLinkKind::PlainUrl);
                }
                TerminalLinkDisambiguationItem::Hyperlink { uri, .. } => {
                    activate_terminal_hyperlink(&terminal_for_action, uri);
                }
            }
        });
        group.add_action(&action);
    }

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.insert_action_group("link", Some(&group));
    popover.set_parent(terminal);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let terminal_for_close = terminal.clone();
    popover.connect_closed(move |popover| {
        let popover = popover.clone();
        let terminal = terminal_for_close.clone();
        glib::idle_add_local_once(move || {
            if popover.parent().is_some() {
                popover.unparent();
            }
            terminal.grab_focus();
        });
    });
    popover.popup();
    true
}

impl TerminalLinkDisambiguationItem {
    fn label(&self) -> &str {
        match self {
            TerminalLinkDisambiguationItem::File { label, .. }
            | TerminalLinkDisambiguationItem::Url { label, .. }
            | TerminalLinkDisambiguationItem::Hyperlink { label, .. } => label,
        }
    }
}

/// Peek the file referenced by the current terminal selection, or — when there
/// is no selection — the token under the terminal cursor. Bound to the
/// `peek-file` action so the user can open a bare path or a `path:line[:col]`
/// reference without touching the mouse. Resolves against the active pane's cwd
/// and toasts when the candidate is not a local file that exists.
pub(crate) fn peek_active_selection(state: &Rc<RefCell<AppState>>) {
    let Some(terminal) = crate::get_active_terminal(state) else {
        return;
    };
    let candidate = active_selection_text(state)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| token_under_cursor(&terminal));
    let Some(candidate) = candidate else {
        crate::show_toast("No file path under cursor to peek");
        return;
    };
    match resolve_local_file_match(&terminal, &candidate) {
        Some((resolved, line, _col)) => crate::peek::peek_path(&resolved, Some(line)),
        None => crate::show_toast("No file path under cursor to peek"),
    }
}

fn build_terminal(cfg: &GhosttyConfig) -> (vte::Terminal, gtk::Box) {
    let terminal = vte::Terminal::new();
    configure_terminal(&terminal, cfg);
    let local_selection = Rc::new(RefCell::new(LocalSelectionState::default()));
    setup_keybindings(&terminal, local_selection.clone());
    sync_input_mode_visual_state(&terminal);
    terminal.set_hexpand(true);
    terminal.set_vexpand(true);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);

    let terminal_overlay = gtk::Overlay::new();
    terminal_overlay.set_hexpand(true);
    terminal_overlay.set_vexpand(true);
    terminal_overlay.set_child(Some(&terminal));

    let selection_overlay = gtk::DrawingArea::new();
    selection_overlay.set_hexpand(true);
    selection_overlay.set_vexpand(true);
    selection_overlay.set_halign(gtk::Align::Fill);
    selection_overlay.set_valign(gtk::Align::Fill);
    selection_overlay.set_can_target(false);
    terminal_overlay.add_overlay(&selection_overlay);

    install_local_selection_overlay_draw(
        &selection_overlay,
        &terminal,
        local_selection.clone(),
        cfg.selection_bg.as_str(),
    );
    install_local_selection_drag_copy(
        &terminal_overlay,
        &terminal,
        &selection_overlay,
        local_selection,
    );
    container.append(&terminal_overlay);

    (terminal, container)
}

fn build_pane_leaf(
    pane_id: u32,
    terminal: &vte::Terminal,
    container: &gtk::Box,
    launch_command: Option<String>,
) -> PaneLeaf {
    PaneLeaf {
        pane_id,
        work_origin: crate::pane::new_pane_work_origin(),
        container: container.clone(),
        terminal: terminal.clone(),
        shell_pid: None,
        was_busy: false,
        launch_command,
        output_tracker: Rc::new(Cell::new(None)),
        tmux_backing: None,
        restored_tmux: None,
        restore_unavailable_reason: None,
        location_state: PaneLocationState::default(),
        process_state: PaneProcessState::default(),
        current_task: None,
        restored_agent_session: None,
        restored_spawn_kind: None,
        agent_resume: None,
        command_marks: Vec::new(),
        broker: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalSelectionDrag {
    start_x: f64,
    start_y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalGridPoint {
    row: libc::c_long,
    col: libc::c_long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalTextRange {
    start_row: libc::c_long,
    start_col: libc::c_long,
    end_row: libc::c_long,
    end_col: libc::c_long,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalSelectionRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalSelectionGeometry {
    visible_start_row: libc::c_long,
    rows: libc::c_long,
    cols: libc::c_long,
    char_width: libc::c_long,
    char_height: libc::c_long,
    x_offset: f64,
    y_offset: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalSelectionCapture {
    range: TerminalTextRange,
    text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct LocalSelectionState {
    range: Option<TerminalTextRange>,
    text: Option<String>,
}

impl LocalSelectionState {
    fn clear(&mut self) {
        self.range = None;
        self.text = None;
    }

    fn set_capture(&mut self, capture: LocalSelectionCapture) {
        self.range = Some(capture.range);
        self.text = Some(capture.text);
    }
}

fn should_capture_plain_selection_drag(mods: gdk::ModifierType) -> bool {
    let mods = mods
        & (gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::META_MASK
            | gdk::ModifierType::SUPER_MASK);

    // Selection-first: a plain primary drag is handled locally, even when the
    // child process has enabled terminal mouse reporting. Modified drags remain
    // available for terminal apps or VTE's native Shift-selection behavior.
    mods.is_empty()
}

fn local_selection_drag_moved_enough(start_x: f64, start_y: f64, end_x: f64, end_y: f64) -> bool {
    let dx = end_x - start_x;
    let dy = end_y - start_y;
    (dx * dx + dy * dy).sqrt() >= LOCAL_SELECTION_DRAG_MIN_DISTANCE_PX
}

fn terminal_grid_point_at(
    visible_start_row: libc::c_long,
    rows: libc::c_long,
    cols: libc::c_long,
    char_width: libc::c_long,
    char_height: libc::c_long,
    x: f64,
    y: f64,
) -> Option<TerminalGridPoint> {
    if rows <= 0 || cols <= 0 || char_width <= 0 || char_height <= 0 {
        return None;
    }

    let col = (x.max(0.0) / char_width as f64).floor() as libc::c_long;
    let row = (y.max(0.0) / char_height as f64).floor() as libc::c_long;

    Some(TerminalGridPoint {
        row: visible_start_row + row.clamp(0, rows - 1),
        col: col.clamp(0, cols - 1),
    })
}

fn terminal_text_range_between_points(
    start: TerminalGridPoint,
    end: TerminalGridPoint,
) -> Option<TerminalTextRange> {
    if start == end {
        return None;
    }

    let (start, end) = if (start.row, start.col) <= (end.row, end.col) {
        (start, end)
    } else {
        (end, start)
    };

    Some(TerminalTextRange {
        start_row: start.row,
        start_col: start.col,
        end_row: end.row,
        end_col: end.col,
    })
}

fn local_selection_rects(
    range: TerminalTextRange,
    geometry: LocalSelectionGeometry,
) -> Vec<LocalSelectionRect> {
    if geometry.rows <= 0
        || geometry.cols <= 0
        || geometry.char_width <= 0
        || geometry.char_height <= 0
    {
        return Vec::new();
    }

    let visible_end_row = geometry.visible_start_row + geometry.rows - 1;
    let start_row = range.start_row.max(geometry.visible_start_row);
    let end_row = range.end_row.min(visible_end_row);
    if start_row > end_row {
        return Vec::new();
    }

    let mut rects = Vec::new();
    for row in start_row..=end_row {
        let start_col = if row == range.start_row {
            range.start_col
        } else {
            0
        }
        .clamp(0, geometry.cols - 1);
        let end_col = if row == range.end_row {
            range.end_col
        } else {
            geometry.cols - 1
        }
        .clamp(0, geometry.cols - 1);

        if start_col > end_col {
            continue;
        }

        rects.push(LocalSelectionRect {
            x: geometry.x_offset + start_col as f64 * geometry.char_width as f64,
            y: geometry.y_offset
                + (row - geometry.visible_start_row) as f64 * geometry.char_height as f64,
            width: (end_col - start_col + 1) as f64 * geometry.char_width as f64,
            height: geometry.char_height as f64,
        });
    }

    rects
}

fn terminal_visible_start_row(terminal: &vte::Terminal) -> libc::c_long {
    if let Some(adjustment) = terminal.vadjustment() {
        adjustment.value().floor().max(0.0) as libc::c_long
    } else {
        let (_cursor_col, cursor_row) = terminal.cursor_position();
        (cursor_row - terminal.row_count() + 1).max(0)
    }
}

/// Scroll the active pane between recorded prompt marks. `Previous` moves to the
/// nearest prompt above the current viewport top; `Next` to the nearest below.
/// The row space of the recorded marks matches `vadjustment().value()` and
/// `terminal_visible_start_row`, so `set_value` (which GTK clamps to the
/// adjustment's range) scrolls the prompt to the top of the view. When the pane
/// has recorded no marks — no shell integration sourced — this shows a one-time
/// hint toast and does nothing.
pub(crate) fn jump_active_pane_to_prompt(state: &Rc<RefCell<AppState>>, dir: PromptJump) {
    let (terminal, marks) = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            return;
        };
        let pane_id = tab.focused_pane_id;
        let Some(terminal) = tab.panes.focused_terminal(pane_id).cloned() else {
            return;
        };
        let marks = tab
            .panes
            .leaf(pane_id)
            .map(|leaf| leaf.command_marks.clone())
            .unwrap_or_default();
        (terminal, marks)
    };

    if marks.is_empty() {
        crate::show_error_toast(
            "No prompt marks yet — source examples/taarof-shell-integration.sh in your shell.",
        );
        return;
    }

    let top = terminal_visible_start_row(&terminal);
    if let Some(target) = broadcast::next_prompt_mark(&marks, top, dir) {
        if let Some(adjustment) = terminal.vadjustment() {
            adjustment.set_value(target as f64);
        }
    }
}

fn terminal_local_selection_capture(
    terminal: &vte::Terminal,
    start_x: f64,
    start_y: f64,
    end_x: f64,
    end_y: f64,
) -> Option<LocalSelectionCapture> {
    if !local_selection_drag_moved_enough(start_x, start_y, end_x, end_y) {
        return None;
    }

    let x_offset = terminal.margin_start() as f64;
    let y_offset = terminal.margin_top() as f64;
    let visible_start_row = terminal_visible_start_row(terminal);
    let rows = terminal.row_count();
    let cols = terminal.column_count();
    let char_width = terminal.char_width();
    let char_height = terminal.char_height();

    let start = terminal_grid_point_at(
        visible_start_row,
        rows,
        cols,
        char_width,
        char_height,
        start_x - x_offset,
        start_y - y_offset,
    )?;
    let end = terminal_grid_point_at(
        visible_start_row,
        rows,
        cols,
        char_width,
        char_height,
        end_x - x_offset,
        end_y - y_offset,
    )?;
    let range = terminal_text_range_between_points(start, end)?;
    let (text, _len) = terminal.text_range_format(
        vte::Format::Text,
        range.start_row,
        range.start_col,
        range.end_row,
        range.end_col,
    );
    let text = text.map(|g| g.to_string()).unwrap_or_default();
    if text.is_empty() {
        None
    } else {
        Some(LocalSelectionCapture { range, text })
    }
}

fn set_local_selection_clipboard_text(text: &str) -> Result<(), String> {
    self::broadcast::set_clipboard_text(text)?;
    if let Some(display) = gdk::Display::default() {
        display.primary_clipboard().set_text(text);
    }
    Ok(())
}

fn set_terminal_input_enabled(terminal: &vte::Terminal, input_enabled: bool, grab_focus: bool) {
    terminal.set_input_enabled(input_enabled);
    sync_input_mode_visual_state(terminal);
    if grab_focus {
        terminal.grab_focus();
    }
}

fn sync_input_mode_visual_state(terminal: &vte::Terminal) {
    if terminal.is_input_enabled() {
        terminal.remove_css_class(SELECTION_MODE_CSS_CLASS);
    } else {
        terminal.add_css_class(SELECTION_MODE_CSS_CLASS);
    }
}

pub(crate) fn toggle_terminal_input_mode(terminal: &vte::Terminal) -> bool {
    let input_enabled = !terminal.is_input_enabled();
    set_terminal_input_enabled(terminal, input_enabled, true);
    input_enabled
}

pub(crate) fn toggle_active_terminal_input_mode(state: &Rc<RefCell<AppState>>) -> Option<bool> {
    let terminal = crate::get_active_terminal(state)?;
    Some(toggle_terminal_input_mode(&terminal))
}

const TAAROF_AGENT_STATE_TERMPROP: &str = "vte.ext.taarof.agent.state";
const TAAROF_AGENT_TEXT_TERMPROP: &str = "vte.ext.taarof.agent.text";
const TAAROF_AGENT_SOURCE_TERMPROP: &str = "vte.ext.taarof.agent.source";

const TAAROF_ACTIVITY_TERMPROPS: &[&str] = &[
    TAAROF_AGENT_STATE_TERMPROP,
    TAAROF_AGENT_TEXT_TERMPROP,
    TAAROF_AGENT_SOURCE_TERMPROP,
];

static TAAROF_TERMPROPS_INSTALLED: Once = Once::new();

/// Install taarof-specific VTE termprops before any VTE terminals are created.
pub(crate) fn install_taarof_activity_termprops() {
    TAAROF_TERMPROPS_INSTALLED.call_once(|| unsafe {
        install_termprop(
            TAAROF_AGENT_STATE_TERMPROP,
            vte::ffi::VTE_PROPERTY_STRING,
            vte::ffi::VTE_PROPERTY_FLAG_NONE,
        );
        install_termprop(
            TAAROF_AGENT_TEXT_TERMPROP,
            vte::ffi::VTE_PROPERTY_STRING,
            vte::ffi::VTE_PROPERTY_FLAG_NONE,
        );
        install_termprop(
            TAAROF_AGENT_SOURCE_TERMPROP,
            vte::ffi::VTE_PROPERTY_STRING,
            vte::ffi::VTE_PROPERTY_FLAG_NONE,
        );
    });
}

unsafe fn install_termprop(
    name: &str,
    type_: vte::ffi::VtePropertyType,
    flags: vte::ffi::VtePropertyFlags,
) {
    let name = CString::new(name).expect("termprop name must not contain NUL");
    let prop_id = vte::ffi::vte_install_termprop(name.as_ptr(), type_, flags);
    if prop_id < 0 {
        eprintln!(
            "taarof: failed to install VTE termprop {}",
            name.to_string_lossy()
        );
    }
}

/// Wire up terminal-level keybindings (copy, paste, palette, splits, close-pane).
/// These are intercepted at the VTE EventControllerKey level because VTE consumes
/// keys before the window-level ShortcutController sees them.
/// Bindings are read from the keybinding config via the thread-local installed at startup.
fn setup_keybindings(terminal: &vte::Terminal, local_selection: Rc<RefCell<LocalSelectionState>>) {
    use crate::keybindings::{get_terminal_bindings, Action};

    let bindings = get_terminal_bindings();
    if bindings.is_empty() {
        return;
    }

    let key_ctrl = gtk::EventControllerKey::new();
    let term = terminal.clone();
    key_ctrl.connect_key_pressed(move |_ctrl, key, code, mods| {
        for binding in &bindings {
            if !binding.matches(key, mods, code) {
                continue;
            }
            match binding.action {
                Action::Copy => {
                    if term.has_selection() {
                        self::broadcast::copy_vte_selection_and_record(&term);
                    } else if let Some(text) = local_selection.borrow().text.clone() {
                        if let Err(err) = set_local_selection_clipboard_text(&text) {
                            eprintln!("taarof: failed to copy local selection: {err}");
                        }
                        self::broadcast::record_clipboard_copy(&text);
                    } else {
                        self::broadcast::copy_vte_selection_and_record(&term);
                    }
                    return glib::Propagation::Stop;
                }
                Action::Paste => {
                    if let Some(root) = term.root() {
                        if let Some(win) = root.downcast_ref::<gtk::Window>() {
                            crate::keybindings::activate(win, crate::keybindings::Action::Paste);
                            return glib::Propagation::Stop;
                        }
                    }
                    term.paste_clipboard();
                    return glib::Propagation::Stop;
                }
                _ => {
                    // Forward to window action (palette, splits, close-pane, etc.)
                    if let Some(root) = term.root() {
                        if let Some(win) = root.downcast_ref::<gtk::Window>() {
                            crate::keybindings::activate(win, binding.action);
                        }
                    }
                    return glib::Propagation::Stop;
                }
            }
        }
        glib::Propagation::Proceed
    });
    terminal.add_controller(key_ctrl);
}

fn install_local_selection_overlay_draw(
    selection_overlay: &gtk::DrawingArea,
    terminal: &vte::Terminal,
    local_selection: Rc<RefCell<LocalSelectionState>>,
    selection_bg: &str,
) {
    let mut highlight = gdk::RGBA::parse(selection_bg)
        .unwrap_or_else(|_| gdk::RGBA::parse("#e0a43a").expect("fallback selection color valid"));
    highlight.set_alpha((highlight.alpha() as f64 * LOCAL_SELECTION_OVERLAY_ALPHA) as f32);

    let terminal_for_draw = terminal.clone();
    selection_overlay.set_draw_func(move |_, cr, _, _| {
        let Some(range) = local_selection.borrow().range else {
            return;
        };

        cr.set_source_rgba(
            highlight.red() as f64,
            highlight.green() as f64,
            highlight.blue() as f64,
            highlight.alpha() as f64,
        );

        let geometry = LocalSelectionGeometry {
            visible_start_row: terminal_visible_start_row(&terminal_for_draw),
            rows: terminal_for_draw.row_count(),
            cols: terminal_for_draw.column_count(),
            char_width: terminal_for_draw.char_width(),
            char_height: terminal_for_draw.char_height(),
            x_offset: terminal_for_draw.margin_start() as f64,
            y_offset: terminal_for_draw.margin_top() as f64,
        };

        for rect in local_selection_rects(range, geometry) {
            cr.rectangle(rect.x, rect.y, rect.width, rect.height);
        }
        if let Err(err) = cr.fill() {
            eprintln!("taarof: failed to draw local selection overlay: {err}");
        }
    });
}

fn install_local_selection_drag_copy<W: IsA<gtk::Widget>>(
    controller_owner: &W,
    terminal: &vte::Terminal,
    selection_overlay: &gtk::DrawingArea,
    local_selection: Rc<RefCell<LocalSelectionState>>,
) {
    let drag_state = Rc::new(Cell::new(None::<LocalSelectionDrag>));
    let selection_click = gtk::GestureClick::new();
    selection_click.set_button(gdk::BUTTON_PRIMARY);
    selection_click.set_propagation_phase(gtk::PropagationPhase::Capture);

    {
        let terminal = terminal.clone();
        let drag_state = drag_state.clone();
        let selection_overlay = selection_overlay.clone();
        let local_selection = local_selection.clone();
        selection_click.connect_pressed(move |gesture, n_press, x, y| {
            local_selection.borrow_mut().clear();
            selection_overlay.queue_draw();

            if n_press != 1 {
                return;
            }

            let current_event = gesture.current_event();
            let mods = current_event
                .as_ref()
                .map(|event| event.modifier_state())
                .unwrap_or(gdk::ModifierType::empty());

            if should_capture_plain_selection_drag(mods) {
                drag_state.set(Some(LocalSelectionDrag {
                    start_x: x,
                    start_y: y,
                }));
                terminal.grab_focus();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            } else {
                drag_state.set(None);
            }
        });
    }

    {
        let terminal = terminal.clone();
        let drag_state = drag_state.clone();
        let selection_overlay = selection_overlay.clone();
        let local_selection = local_selection.clone();
        selection_click.connect_released(move |gesture, _, end_x, end_y| {
            if let Some(drag) = drag_state.take() {
                if let Some(capture) = terminal_local_selection_capture(
                    &terminal,
                    drag.start_x,
                    drag.start_y,
                    end_x,
                    end_y,
                ) {
                    let text = capture.text.clone();
                    local_selection.borrow_mut().set_capture(capture);
                    selection_overlay.queue_draw();
                    if let Err(err) = set_local_selection_clipboard_text(&text) {
                        eprintln!("taarof: failed to copy local drag selection: {err}");
                    }
                    self::broadcast::record_clipboard_copy(&text);
                } else {
                    local_selection.borrow_mut().clear();
                    selection_overlay.queue_draw();
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        });
    }

    {
        let drag_state = drag_state.clone();
        selection_click.connect_unpaired_release(move |gesture, _, _, _, _| {
            if drag_state.take().is_some() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            } else {
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
        });
    }

    controller_owner.add_controller(selection_click);

    let selection_motion = gtk::EventControllerMotion::new();
    selection_motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let terminal = terminal.clone();
        let drag_state = drag_state.clone();
        let selection_overlay = selection_overlay.clone();
        let local_selection = local_selection.clone();
        selection_motion.connect_motion(move |_, x, y| {
            let Some(drag) = drag_state.get() else {
                return;
            };
            if let Some(capture) =
                terminal_local_selection_capture(&terminal, drag.start_x, drag.start_y, x, y)
            {
                local_selection.borrow_mut().set_capture(capture);
                selection_overlay.queue_draw();
            }
        });
    }
    controller_owner.add_controller(selection_motion);

    {
        let selection_overlay = selection_overlay.clone();
        terminal.connect_selection_changed(move |term| {
            if term.has_selection() {
                local_selection.borrow_mut().clear();
                selection_overlay.queue_draw();
            }
        });
    }
}

/// Parse an OSC 7 URI into (hostname, path). URI format: file://hostname/path
pub(crate) fn parse_osc7_uri(uri: &str) -> (Option<String>, String) {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    // First component after file:// is the hostname, rest is path
    if let Some(slash_idx) = stripped.find('/') {
        let host = &stripped[..slash_idx];
        let path = &stripped[slash_idx..];
        let path = urlencoding::decode(path)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| path.to_string());
        let host = is_remote_host(Some(host)).then(|| host.trim().to_string());
        (host, path)
    } else {
        (None, stripped.to_string())
    }
}

/// Check if a hostname is the local machine.
pub(crate) fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" {
        return true;
    }
    let local: String = glib::host_name().into();
    let local_short = local.split('.').next().unwrap_or(&local);
    let host_short = host.split('.').next().unwrap_or(host);
    local_short.eq_ignore_ascii_case(host_short)
}

/// Whether optional pane metadata names a host other than this machine.
/// Missing, empty, and local host spellings all describe local panes.
pub(crate) fn is_remote_host(host: Option<&str>) -> bool {
    host.map(str::trim)
        .filter(|host| !host.is_empty())
        .is_some_and(|host| !is_local_host(host))
}

/// Shorten a path for display: replace $HOME with ~, take last 2 components if long.
fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if display.chars().count() > 30 {
        let parts: Vec<&str> = display.rsplitn(3, '/').collect();
        if parts.len() >= 2 {
            let tail: Vec<&str> = parts[..2].iter().rev().copied().collect();
            format!("…/{}", tail.join("/"))
        } else {
            display
        }
    } else {
        display
    }
}

/// Format hostname + path for sidebar display.
fn format_cwd_display(host: Option<&str>, path: &str) -> String {
    let short = shorten_path(path);
    match host {
        Some(h) => format!("{h}:{short}"),
        None => short,
    }
}

/// Extract a taarof-managed git branch from the terminal title, if present.
/// Format: "... [taarof-git:<branch>]"
fn extract_taarof_branch_from_title(title: &str) -> Option<String> {
    let marker = " [taarof-git:";
    let start = title.rfind(marker)?;
    if !title.ends_with(']') {
        return None;
    }
    let branch = &title[start + marker.len()..title.len() - 1];
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

fn strip_taarof_branch_suffix(title: &str) -> &str {
    let marker = " [taarof-git:";
    if let Some(start) = title.rfind(marker) {
        if title.ends_with(']') {
            return &title[..start];
        }
    }
    title
}

fn extract_host_from_title(title: &str) -> Option<String> {
    let title = strip_taarof_branch_suffix(title);
    let colon_idx = title.find(':')?;
    let login = &title[..colon_idx];
    let at_idx = login.rfind('@')?;
    let host = login[at_idx + 1..].trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// Try to extract a path from a terminal title (common formats: "user@host:path", "path - shell")
fn extract_path_from_title(title: &str) -> Option<String> {
    let title = strip_taarof_branch_suffix(title);
    // "user@host:~/path" or "user@host:/path"
    if let Some(colon_idx) = title.find(':') {
        if title[..colon_idx].contains('@') {
            let path = &title[colon_idx + 1..];
            // Strip trailing shell info like " - zsh"
            let path = path.split(" - ").next().unwrap_or(path).trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

pub(crate) fn location_metadata_from_terminal_state(
    cwd_uri: Option<&str>,
    title: Option<&str>,
) -> (Option<String>, Option<String>) {
    if let Some(uri) = cwd_uri {
        let (host, path) = parse_osc7_uri(uri);
        return (Some(path), host);
    }

    (
        title.and_then(extract_path_from_title),
        title.and_then(extract_host_from_title),
    )
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub(crate) fn terminal_location_metadata(
    terminal: &vte::Terminal,
) -> (Option<String>, Option<String>) {
    let cwd_uri = terminal.current_directory_uri().map(|uri| uri.to_string());
    let title = terminal.window_title().map(|title| title.to_string());
    location_metadata_from_terminal_state(cwd_uri.as_deref(), title.as_deref())
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cache_pane_location(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    cwd: Option<String>,
    cwd_host: Option<String>,
) {
    let mut st = state.borrow_mut();
    if let Some(tab) = st.find_tab_mut(tab_id) {
        if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
            leaf.location_state = PaneLocationState {
                cwd,
                cwd_host,
                updated_at_unix_ms: Some(unix_time_ms()),
            };
        }
    }
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
fn refresh_pane_location_cache_from_terminal(
    state: &Rc<RefCell<AppState>>,
    terminal: &vte::Terminal,
    tab_id: u32,
    pane_id: u32,
) {
    let (cwd, cwd_host) = terminal_location_metadata(terminal);
    cache_pane_location(state, tab_id, pane_id, cwd, cwd_host);
}

/// Read the current git branch from the .git/HEAD file in the given directory.
/// Returns the branch name if on a branch, or a short hash if in detached HEAD state.
///
/// This is a thin wrapper around [`crate::git::discover`] so the branch
/// resolution follows the linked-worktree `gitdir:` pointer. Reading
/// `<dir>/.git/HEAD` directly would either return the main checkout's HEAD
/// (when the path is a linked worktree and `.git` is a file containing the
/// `gitdir:` line) or fail outright.
pub fn read_git_branch(cwd: &str) -> Option<String> {
    crate::git::discover(cwd).branch
}

fn local_path_from_uri(uri: &str) -> Option<String> {
    let (host, path) = parse_osc7_uri(uri);
    if host.is_some() {
        // Remote OSC 7 paths are not readable locally; rely on title metadata instead.
        None
    } else {
        Some(crate::git::normalize_path(&path))
    }
}

fn set_branch_label(branch_label: &gtk::Label, branch: Option<&str>) {
    if let Some(branch) = branch.filter(|branch| !branch.is_empty()) {
        branch_label.set_text(branch);
        branch_label.set_visible(!crate::sidebar::compact_mode_active());
    } else {
        branch_label.set_text("");
        branch_label.set_visible(false);
    }
}

/// Read a branch only on the bounded Git worker. Each worker key includes the
/// label's current cwd generation: a leave-and-return to the same path is a
/// fresh request, never a coalesced completion from the earlier cwd epoch.
fn request_branch_label_refresh(branch_label: &gtk::Label, cwd: String) {
    let request = BRANCH_LABEL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let label_key = branch_label.as_ptr() as usize;
        if cache
            .current
            .get(&label_key)
            .is_some_and(|current| current.cwd == cwd)
        {
            if let Some(branch) = cache.values.get(&cwd) {
                set_branch_label(branch_label, branch.as_deref());
            }
            return None;
        }
        cache.next_generation = cache.next_generation.wrapping_add(1);
        let generation = cache.next_generation;
        cache.current.insert(
            label_key,
            BranchLabelCurrent {
                label: branch_label.downgrade(),
                cwd: cwd.clone(),
                generation,
            },
        );
        if let Some(branch) = cache.values.get(&cwd) {
            set_branch_label(branch_label, branch.as_deref());
            return None;
        }
        let request = BranchLabelRequest {
            cwd: cwd.clone(),
            generation,
        };
        let waiters = cache.waiters.entry(request.clone()).or_default();
        waiters.push(BranchLabelWaiter {
            label: branch_label.downgrade(),
            label_key,
            generation,
        });
        Some(request)
    });
    let Some(request) = request else {
        return;
    };

    let worker_cwd = request.cwd.clone();
    let request_for_apply = request.clone();
    let submission = crate::git::spawn_async_result(
        format!("branch-label:{}:{}", request.cwd, request.generation),
        move || Ok(read_git_branch(&worker_cwd)),
        move |result| {
            let waiters = BRANCH_LABEL_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache
                    .current
                    .retain(|_, current| current.label.upgrade().is_some());
                let waiters = cache.waiters.remove(&request_for_apply).unwrap_or_default();
                let branch = match result {
                    Ok(branch) => branch,
                    Err(error) => {
                        // Drain waiters on every completion, but never turn a
                        // failed join into a cached empty/no-branch fact.
                        for waiter in waiters {
                            if cache.current.get(&waiter.label_key).is_some_and(|current| {
                                current.cwd == request_for_apply.cwd
                                    && current.generation == waiter.generation
                            }) {
                                cache.current.remove(&waiter.label_key);
                            }
                        }
                        crate::show_error_toast(&format!("Branch label worker failed: {error}"));
                        return Vec::new();
                    }
                };
                let has_current_waiter = waiters.iter().any(|waiter| {
                    cache.current.get(&waiter.label_key).is_some_and(|current| {
                        current.cwd == request_for_apply.cwd
                            && current.generation == waiter.generation
                    })
                });
                if !has_current_waiter {
                    return Vec::new();
                }
                if cache.values.len() >= BRANCH_LABEL_CACHE_CAPACITY
                    && !cache.values.contains_key(&request_for_apply.cwd)
                {
                    // Bounded display cache: completed values are disposable,
                    // while in-flight entries remain in `waiters` until this
                    // apply phase drains them.
                    if let Some(evicted) = cache.values.keys().next().cloned() {
                        cache.values.remove(&evicted);
                    }
                }
                cache
                    .values
                    .insert(request_for_apply.cwd.clone(), branch.clone());
                waiters
            });
            let branch = BRANCH_LABEL_CACHE.with(|cache| {
                cache
                    .borrow()
                    .values
                    .get(&request_for_apply.cwd)
                    .cloned()
                    .flatten()
            });
            for waiter in waiters {
                let is_current = BRANCH_LABEL_CACHE.with(|cache| {
                    cache
                        .borrow()
                        .current
                        .get(&waiter.label_key)
                        .is_some_and(|current| {
                            current.cwd == request_for_apply.cwd
                                && current.generation == waiter.generation
                        })
                });
                if is_current {
                    if let Some(label) = waiter.label.upgrade() {
                        set_branch_label(&label, branch.as_deref());
                    }
                }
            }
        },
    );
    if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
        // Do not cache saturation as an empty/no-branch result.  A later OSC 7
        // or title update can retry after another bounded worker completes.
        BRANCH_LABEL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            for waiter in cache.waiters.remove(&request).unwrap_or_default() {
                if cache.current.get(&waiter.label_key).is_some_and(|current| {
                    current.cwd == request.cwd && current.generation == waiter.generation
                }) {
                    cache.current.remove(&waiter.label_key);
                }
            }
        });
    }
}

/// Update a branch label based on terminal state.
fn update_branch_label(branch_label: &gtk::Label, uri: Option<&str>, title: Option<&str>) {
    if let Some(branch) = title.and_then(extract_taarof_branch_from_title) {
        BRANCH_LABEL_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .current
                .remove(&(branch_label.as_ptr() as usize));
        });
        set_branch_label(branch_label, Some(&branch));
        return;
    }

    if let Some(cwd) = uri.and_then(local_path_from_uri) {
        // A cache miss deliberately paints no speculative branch.  The worker
        // will update every still-live label after the real Git read completes.
        set_branch_label(branch_label, None);
        request_branch_label_refresh(branch_label, cwd);
    } else {
        BRANCH_LABEL_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .current
                .remove(&(branch_label.as_ptr() as usize));
        });
        set_branch_label(branch_label, None);
    }
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
fn update_tab_labels_from_terminal(
    terminal: &vte::Terminal,
    cwd_label: &gtk::Label,
    branch_label: &gtk::Label,
) {
    let title = terminal.window_title().map(|s| s.to_string());
    if let Some(uri) = terminal.current_directory_uri() {
        let (host, path) = parse_osc7_uri(&uri);
        cwd_label.set_text(&format_cwd_display(host.as_deref(), &path));
        update_branch_label(branch_label, Some(&uri), title.as_deref());
        return;
    }

    update_branch_label(branch_label, None, title.as_deref());
    if let Some(title) = title {
        if let Some(path) = extract_path_from_title(&title) {
            cwd_label.set_text(&shorten_path(&path));
        }
    }
}

fn tmux_target_for_host(host: Option<&crate::host::HostConfig>) -> crate::tmux::TmuxTarget {
    match host.and_then(|h| h.ssh_target.as_deref()) {
        Some(ssh) => crate::tmux::TmuxTarget::Remote {
            ssh_target: ssh.to_string(),
        },
        None => crate::tmux::TmuxTarget::Local,
    }
}

fn explicit_tmux_target_label(host: Option<&crate::host::HostConfig>) -> String {
    match host {
        Some(host) if host.ssh_target.is_some() => format!("remote host {}", host.name),
        _ => "this machine".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitTmuxTabUiState {
    pub available: bool,
    pub local_target_available: bool,
    pub has_remote_targets: bool,
    pub tooltip: String,
    pub unavailable_message: Option<String>,
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn local_command_exists(command: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .any(|candidate| is_executable_file(&candidate))
        })
        .unwrap_or(false)
}

fn explicit_tmux_tab_ui_state_for(
    local_target_available: bool,
    has_remote_targets: bool,
) -> ExplicitTmuxTabUiState {
    match (local_target_available, has_remote_targets) {
        (true, _) => ExplicitTmuxTabUiState {
            available: true,
            local_target_available,
            has_remote_targets,
            tooltip: "Create a tmux-backed tab".to_string(),
            unavailable_message: None,
        },
        (false, true) => ExplicitTmuxTabUiState {
            available: true,
            local_target_available,
            has_remote_targets,
            tooltip: "Local tmux is unavailable on this machine, but you can still create a tmux-backed tab on a configured remote host.".to_string(),
            unavailable_message: None,
        },
        (false, false) => {
            let message = "tmux is not available on this machine and no remote tmux hosts are configured. Install tmux or add a Host entry to ~/.ssh/config to create tmux-backed tabs.".to_string();
            ExplicitTmuxTabUiState {
                available: false,
                local_target_available,
                has_remote_targets,
                tooltip: message.clone(),
                unavailable_message: Some(message),
            }
        }
    }
}

pub fn explicit_tmux_tab_ui_state() -> ExplicitTmuxTabUiState {
    // Candidates come from the shared seam, so a machine with only
    // `~/.ssh/config` aliases (no `[hosts.*]`) still has remote tmux targets.
    let has_remote_targets = !crate::ssh_config::candidates().is_empty();
    explicit_tmux_tab_ui_state_for(local_command_exists("tmux"), has_remote_targets)
}

fn host_config_by_name(name: Option<&str>) -> Option<crate::host::HostConfig> {
    let name = name?;
    crate::config::host_config(name)
}

fn validate_tmux_async<G>(
    target: crate::tmux::TmuxTarget,
    guard: crate::tmux::TmuxGtkApplyGuard,
    callback: G,
) where
    G: FnOnce(Result<(), String>) + 'static,
{
    let argv = crate::tmux::version_command(&target);
    let worker = crate::tmux::default_worker();
    glib::spawn_future_local(async move {
        let result = worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::ValidateTarget(target),
                vec![argv],
                Duration::from_secs(10),
            )
            .await
            .and_then(|completion| {
                if guard.upgrade(&completion).is_none() {
                    return Err("tmux validation was superseded".to_string());
                }
                completion
                    .outcomes()
                    .first()
                    .ok_or_else(|| "tmux validation returned no result".to_string())?
                    .result()
                    .map(|_| ())
            });
        callback(result);
    });
}

fn workspace_tmux_target(
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
) -> Option<crate::tmux::TmuxTarget> {
    let host_config_name = {
        let st = state.borrow();
        let workspace = st.workspaces.iter().find(|ws| ws.id == ws_id)?;
        if !workspace.tmux_backed || !crate::config::tmux_config().enabled {
            return None;
        }
        workspace.host_config_name.clone()
    };

    let host_config = host_config_by_name(host_config_name.as_deref());
    Some(tmux_target_for_host(host_config.as_ref()))
}

fn split_tmux_target_for_tab(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) -> Option<crate::tmux::TmuxTarget> {
    let (host_config_name, pane_tmux_target) = {
        let st = state.borrow();
        let (workspace, tab) = st.find_tab(tab_id)?;
        let existing_pane_id = resolved_target_pane_id(&tab.panes, tab.focused_pane_id)?;
        let leaf = tab.panes.leaf(existing_pane_id)?;
        if !(workspace.tmux_backed || leaf.tmux_backing.is_some())
            || !crate::config::tmux_config().enabled
        {
            return None;
        }
        (
            workspace.host_config_name.clone(),
            leaf.tmux_backing
                .as_ref()
                .map(|backing| backing.target.clone()),
        )
    };

    let host_config = host_config_by_name(host_config_name.as_deref());
    if let Some(host_config) = host_config.as_ref() {
        Some(tmux_target_for_host(Some(host_config)))
    } else if let Some(target) = pane_tmux_target {
        Some(target)
    } else {
        Some(crate::tmux::TmuxTarget::Local)
    }
}

pub fn validate_explicit_tmux_target_async(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
    host: Option<crate::host::HostConfig>,
    callback: impl FnOnce(Result<(), String>) + 'static,
) {
    let target = tmux_target_for_host(host.as_ref());
    let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
    validate_tmux_async(target, guard, move |result| {
        callback(result.map_err(|_| match host.as_ref() {
            Some(host) if host.ssh_target.is_some() => format!(
                "Could not verify tmux on remote host {}. Check SSH access and ensure tmux is installed there.",
                host.name
            ),
            _ => "tmux is not available on this machine. Install tmux to create tmux-backed tabs."
                .to_string(),
        }))
    });
}

pub fn validate_workspace_tmux_then(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
    ws_id: u32,
    on_success: impl FnOnce() + 'static,
) {
    if let Some(target) = workspace_tmux_target(state, ws_id) {
        let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
        validate_tmux_async(target, guard, move |result| match result {
            Ok(()) => on_success(),
            Err(err) => crate::show_error_toast(&err),
        });
    } else {
        on_success();
    }
}

pub fn validate_split_tmux_then(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    on_success: impl FnOnce() + 'static,
) {
    if let Some(target) = split_tmux_target_for_tab(state, tab_id) {
        let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
        validate_tmux_async(target, guard, move |result| match result {
            Ok(()) => on_success(),
            Err(err) => crate::show_error_toast(&err),
        });
    } else {
        on_success();
    }
}

fn focus_menu_target_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    terminal: &vte::Terminal,
) {
    {
        let mut st = state.borrow_mut();
        let _ = st.activate_tab(tab_id);
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.focused_pane_id = pane_id;
        }
    }
    terminal.grab_focus();
}

fn install_pane_context_menu(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) {
    let group = gio::SimpleActionGroup::new();

    let copy_action = gio::SimpleAction::new("copy", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        copy_action.connect_activate(move |_, _| {
            // Focus first so the recorded source label reflects this pane.
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            self::broadcast::copy_vte_selection_and_record(&terminal);
        });
    }
    group.add_action(&copy_action);

    let copy_recent_output_action = gio::SimpleAction::new("copy-recent-output", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        copy_recent_output_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            if let Err(err) = copy_recent_output_for_pane(&state, tab_id, pane_id) {
                crate::show_error_toast(&err);
            }
        });
    }
    group.add_action(&copy_recent_output_action);

    let copy_last_message_action = gio::SimpleAction::new("copy-last-message", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        copy_last_message_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::CopyLastMessage);
        });
    }
    group.add_action(&copy_last_message_action);

    let relay_last_message_action = gio::SimpleAction::new("relay-last-message", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        relay_last_message_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::RelayLastMessage);
        });
    }
    group.add_action(&relay_last_message_action);

    let paste_action = gio::SimpleAction::new("paste", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        paste_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            paste_clipboard_to_active_scope(&state);
        });
    }
    group.add_action(&paste_action);

    let toggle_selection_mode_action = gio::SimpleAction::new("toggle-selection-mode", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        toggle_selection_mode_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            toggle_terminal_input_mode(&terminal);
        });
    }
    group.add_action(&toggle_selection_mode_action);

    let split_vertical_action = gio::SimpleAction::new("split-vertical", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        split_vertical_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::SplitVertical);
        });
    }
    group.add_action(&split_vertical_action);

    let split_horizontal_action = gio::SimpleAction::new("split-horizontal", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        split_horizontal_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::SplitHorizontal);
        });
    }
    group.add_action(&split_horizontal_action);

    let close_action = gio::SimpleAction::new("close", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        close_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::ClosePane);
        });
    }
    group.add_action(&close_action);

    let zoom_action = gio::SimpleAction::new("zoom", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        zoom_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::TogglePaneZoom);
        });
    }
    group.add_action(&zoom_action);

    let search_action = gio::SimpleAction::new("search", None);
    {
        let terminal = terminal.clone();
        let state = state.clone();
        search_action.connect_activate(move |_, _| {
            focus_menu_target_pane(&state, tab_id, pane_id, &terminal);
            crate::keybindings::activate(&terminal, crate::keybindings::Action::SearchToggle);
        });
    }
    group.add_action(&search_action);

    let menu = gio::Menu::new();
    let clipboard = gio::Menu::new();
    clipboard.append(Some("Copy"), Some("pane.copy"));
    clipboard.append(Some("Copy Recent Output"), Some("pane.copy-recent-output"));
    clipboard.append(
        Some("Copy Last Agent Message"),
        Some("pane.copy-last-message"),
    );
    clipboard.append(
        Some("Relay Last Message to Pane…"),
        Some("pane.relay-last-message"),
    );
    clipboard.append(Some("Paste"), Some("pane.paste"));
    clipboard.append(
        Some("Toggle Selection Mode"),
        Some("pane.toggle-selection-mode"),
    );
    menu.append_section(None, &clipboard);

    let layout = gio::Menu::new();
    layout.append(Some("Split Vertical"), Some("pane.split-vertical"));
    layout.append(Some("Split Horizontal"), Some("pane.split-horizontal"));
    layout.append(Some("Close Pane"), Some("pane.close"));
    layout.append(Some("Zoom Pane"), Some("pane.zoom"));
    menu.append_section(None, &layout);

    let search = gio::Menu::new();
    search.append(Some("Search Scrollback"), Some("pane.search"));
    menu.append_section(None, &search);

    terminal.insert_action_group("pane", Some(&group));
    terminal.set_context_menu_model(Some(&menu));
}

fn connect_restore_legend_dismiss_on_interaction(
    terminal: &vte::Terminal,
    tab_id: u32,
    restore_legend_dismiss: Option<&Rc<dyn Fn(u32)>>,
) {
    let Some(restore_legend_dismiss) = restore_legend_dismiss.cloned() else {
        return;
    };

    let key_ctrl = gtk::EventControllerKey::new();
    let dismiss_for_keys = restore_legend_dismiss.clone();
    key_ctrl.connect_key_pressed(move |_, _, _, _| {
        dismiss_for_keys(tab_id);
        glib::Propagation::Proceed
    });
    terminal.add_controller(key_ctrl);

    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.connect_pressed(move |_, _, _, _| {
        restore_legend_dismiss(tab_id);
    });
    terminal.add_controller(click);
}

fn wire_pane_terminal_internal(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    pane_id: u32,
    restore_legend_dismiss: Option<Rc<dyn Fn(u32)>>,
) {
    let Some(labels) = crate::sidebar::find_tab_labels(tab_list, tab_id) else {
        return;
    };
    let (terminal, output_tracker) = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return;
        };
        let Some(leaf) = tab
            .panes
            .leaves()
            .into_iter()
            .find(|leaf| leaf.pane_id == pane_id)
        else {
            return;
        };
        (leaf.terminal.clone(), leaf.output_tracker.clone())
    };

    refresh_pane_location_cache_from_terminal(state, &terminal, tab_id, pane_id);

    install_pane_context_menu(&terminal, state, tab_id, pane_id);
    connect_broadcast_input(&terminal, state);
    connect_restore_legend_dismiss_on_interaction(
        &terminal,
        tab_id,
        restore_legend_dismiss.as_ref(),
    );

    connect_output_tracking(
        &terminal,
        &output_tracker,
        state,
        term_stack,
        tab_list,
        tab_id,
        pane_id,
    );

    connect_pane_focus_tracking(&terminal, state, tab_id, pane_id, &labels);
    connect_cwd_tracking(
        &terminal,
        state,
        tab_id,
        pane_id,
        tab_list,
        &labels.cwd_label,
        &labels.branch_label,
    );
    connect_activity_termprop_protocol(&terminal, state, term_stack, tab_list, tab_id, pane_id);
    connect_command_mark_tracking(&terminal, state, tab_id, pane_id);
    connect_notifications(&terminal, state, tab_id, window);
    connect_child_exit_cleanup(&terminal, state, term_stack, tab_list, tab_id, pane_id);

    let should_refresh = {
        let st = state.borrow();
        st.find_tab(tab_id)
            .is_some_and(|(_, t)| t.focused_pane_id == pane_id)
    };
    if should_refresh {
        update_tab_labels_from_terminal(&terminal, &labels.cwd_label, &labels.branch_label);
    }
}

pub fn wire_pane_terminal(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    pane_id: u32,
) {
    wire_pane_terminal_internal(state, term_stack, tab_list, window, tab_id, pane_id, None);
}

pub fn wire_tab_terminals(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
) {
    wire_tab_terminals_with_restore_dismiss(state, term_stack, tab_list, window, tab_id, None);
}

pub fn wire_tab_terminals_with_restore_dismiss(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    restore_legend_dismiss: Option<Rc<dyn Fn(u32)>>,
) {
    let pane_ids: Vec<u32> = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return;
        };
        tab.panes
            .leaves()
            .into_iter()
            .map(|leaf| leaf.pane_id)
            .collect()
    };
    for pane_id in pane_ids {
        wire_pane_terminal_internal(
            state,
            term_stack,
            tab_list,
            window,
            tab_id,
            pane_id,
            restore_legend_dismiss.clone(),
        );
    }
}

fn schedule_paned_ratio(paned: &gtk::Paned, orientation: gtk::Orientation, ratio: f64) {
    let ratio = ratio.clamp(0.0, 1.0);
    let total = match orientation {
        gtk::Orientation::Horizontal => paned.width(),
        gtk::Orientation::Vertical => paned.height(),
        _ => 0,
    };
    if total > 0 {
        paned.set_position(((total as f64) * ratio) as i32);
        return;
    }

    let paned_for_alloc = paned.clone();
    glib::idle_add_local_once(move || {
        let total = match orientation {
            gtk::Orientation::Horizontal => paned_for_alloc.width(),
            gtk::Orientation::Vertical => paned_for_alloc.height(),
            _ => 0,
        };
        if total > 0 {
            paned_for_alloc.set_position(((total as f64) * ratio) as i32);
        }
    });
}

fn schedule_paned_zoom_position(
    paned: &gtk::Paned,
    orientation: gtk::Orientation,
    target_child: SplitChild,
) {
    let total = match orientation {
        gtk::Orientation::Horizontal => paned.width(),
        gtk::Orientation::Vertical => paned.height(),
        _ => 0,
    };
    if total > 0 {
        let position = match target_child {
            SplitChild::First => total,
            SplitChild::Second => 0,
        };
        paned.set_position(position);
        return;
    }

    let paned_for_alloc = paned.clone();
    glib::idle_add_local_once(move || {
        let total = match orientation {
            gtk::Orientation::Horizontal => paned_for_alloc.width(),
            gtk::Orientation::Vertical => paned_for_alloc.height(),
            _ => 0,
        };
        if total > 0 {
            let position = match target_child {
                SplitChild::First => total,
                SplitChild::Second => 0,
            };
            paned_for_alloc.set_position(position);
        }
    });
}

fn current_paned_ratio(paned: &gtk::Paned) -> f64 {
    let orientation = paned.orientation();
    let total = match orientation {
        gtk::Orientation::Horizontal => paned.width(),
        gtk::Orientation::Vertical => paned.height(),
        _ => 0,
    };
    if total > 0 {
        paned.position() as f64 / total as f64
    } else {
        0.5
    }
}

fn build_pane_zoom_state(tab: &Tab) -> Option<crate::workspace::PaneZoomState> {
    let ancestors: Vec<crate::workspace::PaneZoomAncestor> = tab
        .panes
        .split_path_to(tab.focused_pane_id)?
        .into_iter()
        .map(|entry| crate::workspace::PaneZoomAncestor {
            ratio: current_paned_ratio(&entry.widget),
            widget: entry.widget,
            target_child: entry.target_child,
        })
        .collect();

    if ancestors.is_empty() {
        None
    } else {
        Some(crate::workspace::PaneZoomState {
            pane_id: tab.focused_pane_id,
            ancestors,
        })
    }
}

fn apply_pane_zoom_state(zoom_state: &crate::workspace::PaneZoomState) {
    for ancestor in &zoom_state.ancestors {
        let orientation = ancestor.widget.orientation();
        schedule_paned_zoom_position(&ancestor.widget, orientation, ancestor.target_child);
    }
}

fn restore_pane_zoom_state(zoom_state: &crate::workspace::PaneZoomState) {
    for ancestor in &zoom_state.ancestors {
        let orientation = ancestor.widget.orientation();
        schedule_paned_ratio(&ancestor.widget, orientation, ancestor.ratio);
    }
}

pub fn clear_pane_zoom(state: &Rc<RefCell<AppState>>, tab_id: u32) -> bool {
    let mut st = state.borrow_mut();
    let Some(tab) = st.find_tab_mut(tab_id) else {
        return false;
    };
    let Some(zoom_state) = tab.pane_zoom.take() else {
        return false;
    };
    restore_pane_zoom_state(&zoom_state);
    true
}

pub fn toggle_pane_zoom(state: &Rc<RefCell<AppState>>) -> bool {
    let (tab_id, pane_id) = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            return false;
        };
        (tab.id, tab.focused_pane_id)
    };

    let mut st = state.borrow_mut();
    let Some(tab) = st.find_tab_mut(tab_id) else {
        return false;
    };

    let mut changed = false;
    if let Some(zoom_state) = tab.pane_zoom.take() {
        restore_pane_zoom_state(&zoom_state);
        changed = true;
        if zoom_state.pane_id == pane_id {
            return true;
        }
    }

    let Some(next_zoom_state) = build_pane_zoom_state(tab) else {
        return changed;
    };
    apply_pane_zoom_state(&next_zoom_state);
    tab.pane_zoom = Some(next_zoom_state);
    true
}

/// Focus the adjacent pane in the specified spatial direction.
pub fn focus_pane_in_direction(
    state: &Rc<RefCell<AppState>>,
    _term_stack: &gtk::Stack,
    direction: PaneNavigationDirection,
    _window: &adw::ApplicationWindow,
) -> bool {
    let (tab_id, _current_pane_id, target_pane_id) = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            return false;
        };
        let current_id = tab.focused_pane_id;
        // Find adjacent pane using the tree navigation method
        let target_id = tab.panes.find_adjacent_pane(current_id, direction);
        (tab.id, current_id, target_id)
    };

    let Some(target_id) = target_pane_id else {
        // No adjacent pane in this direction
        return false;
    };

    let terminal = {
        let mut st = state.borrow_mut();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return false;
        };
        let Some(leaf) = tab.panes.leaf(target_id) else {
            return false;
        };
        let terminal = leaf.terminal.clone();
        tab.focused_pane_id = target_id;
        terminal
    };

    // GTK emits focus-enter synchronously; its pane tracker borrows AppState.
    // Release our borrow before entering GTK so directional focus can re-enter.
    terminal.grab_focus();
    true
}

pub fn restore_tab(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    saved_panes: Option<&SavedPaneNode>,
    fallback_cwd: Option<&str>,
    auto_resume_agents: AutoResumeAgents,
    show_restore_legend: bool,
) -> u32 {
    let cfg = crate::config::ghostty_config();
    let mut next_pane_id = 0u32;
    let mut spawns = Vec::new();
    let normalized_saved = saved_panes.cloned().map(|mut saved| {
        saved.normalize_work_origins();
        saved
    });

    let panes = if let Some(saved) = normalized_saved.as_ref() {
        build_restored_pane_tree(
            saved,
            &cfg,
            &mut next_pane_id,
            &mut spawns,
            auto_resume_agents.enabled(),
        )
    } else {
        let (terminal, container) = build_terminal(&cfg);
        spawns.push((0, terminal.clone(), saved_cwd_to_path(fallback_cwd), None));
        next_pane_id = 1;
        PaneNode::Leaf(build_pane_leaf(0, &terminal, &container, None))
    };

    let focused_pane_id = panes
        .first_leaf()
        .expect("a restored terminal pane tree always contains a leaf")
        .pane_id;
    let root_widget = panes
        .root_widget()
        .expect("a restored terminal pane tree always has a root widget");
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(panes),
        focused_pane_id,
        next_pane_id,
        close_on_exit: true,
        respawn_on_exit: None,
    });
    if let Some(legend) = show_restore_legend
        .then_some(normalized_saved.as_ref())
        .flatten()
        .and_then(|saved| build_restored_tab_legend(saved, tab_id))
    {
        runtime.remember_restored_legend(legend);
    }

    let stack_name = tab_root_widget_name(tab_id);
    term_stack.add_named(&root_widget, Some(&stack_name));

    let state = runtime.shared_state();
    spawn_restored_pane_processes(&state, &cfg, tab_id, spawns);

    tab_id
}

/// Spawn the shell/SSH/tmux processes for a freshly built restored pane tree.
/// SSH and tmux leaves carry an explicit command (from `build_restored_pane_tree`);
/// everything else runs the configured default shell in the saved cwd.
fn spawn_restored_pane_processes(
    state: &Rc<RefCell<AppState>>,
    cfg: &GhosttyConfig,
    tab_id: u32,
    spawns: Vec<RestoredPaneSpawn>,
) {
    let default_argv = build_shell_argv(cfg);
    for (pane_id, terminal, cwd, ssh_cmd) in spawns {
        let (argv, spawn_cwd) = if let Some(ssh_args) = ssh_cmd {
            // SSH tabs: run the restored SSH command and, when possible, replay
            // the saved remote directory on the far side of the connection.
            (ssh_args, None)
        } else {
            (default_argv.clone(), cwd.as_deref().map(String::from))
        };
        spawn_terminal_process(
            &terminal,
            state,
            tab_id,
            pane_id,
            spawn_cwd.as_deref(),
            argv,
        );
    }
}

/// Register a lazily-restored tab: a lightweight `PaneNode::Empty` tab whose
/// real VTE tree and shell processes are deferred until first activation. The
/// sidebar row and session model stay complete; the saved layout is stashed in
/// [`AppState::pending_tab_restores`] and rebuilt by [`materialize_pending_tab`].
///
/// Unlike [`restore_tab`], this does NOT add a term-stack child, wire pane
/// terminals, or spawn any process.
pub fn register_lazy_restored_tab(
    runtime: &RuntimeHandle,
    name: &str,
    saved_panes: Option<&SavedPaneNode>,
    fallback_cwd: Option<&str>,
    show_restore_legend: bool,
) -> u32 {
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(PaneNode::Empty),
        focused_pane_id: 0,
        next_pane_id: 0,
        close_on_exit: true,
        respawn_on_exit: None,
    });

    // Preserve the restore legend exactly like `restore_tab` — it surfaces only
    // once the tab becomes active, so remembering it now is safe for a hidden tab.
    if let Some(legend) = show_restore_legend
        .then_some(saved_panes)
        .flatten()
        .and_then(|saved| build_restored_tab_legend(saved, tab_id))
    {
        runtime.remember_restored_legend(legend);
    }

    let mut saved = saved_panes.cloned().unwrap_or_else(|| SavedPaneNode::Leaf {
        work_origin: None,
        cwd: fallback_cwd.map(str::to_string),
        ssh_command: None,
        tmux_session: None,
        tmux_host: None,
        tmux_identity: None,
        current_task: None,
        agent_session: None,
    });
    saved.normalize_work_origins();
    runtime
        .shared_state()
        .borrow_mut()
        .pending_tab_restores
        .insert(
            tab_id,
            crate::session::PendingTabRestore {
                saved,
                cwd: fallback_cwd.map(str::to_string),
                show_restore_legend,
            },
        );

    tab_id
}

/// Build and spawn a pending (lazily restored) tab on first activation.
///
/// Removes the tab's stashed layout, reconstructs the real VTE pane tree, adds
/// it to the terminal stack, wires the terminals, and spawns each pane's
/// process — mirroring [`restore_tab`] plus the restore-loop wiring. Returns
/// `false` (a no-op) when the tab has no pending restore, so it is safe to call
/// unconditionally from every activation path.
pub fn materialize_pending_tab(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    restore_legend_dismiss: Option<Rc<dyn Fn(u32)>>,
) -> bool {
    let pending = match state.borrow_mut().pending_tab_restores.remove(&tab_id) {
        Some(pending) => pending,
        None => return false,
    };

    let cfg = crate::config::ghostty_config();
    let mut next_pane_id = 0u32;
    let mut spawns = Vec::new();
    let panes = build_restored_pane_tree(
        &pending.saved,
        &cfg,
        &mut next_pane_id,
        &mut spawns,
        SessionRestoreResumePolicy::lazy(
            crate::config::should_auto_resume_agents_on_session_restore(),
        )
        .enabled(),
    );
    let focused_pane_id = panes
        .first_leaf()
        .expect("a pending terminal pane tree always contains a leaf")
        .pane_id;
    let root_widget = panes
        .root_widget()
        .expect("a pending terminal pane tree always has a root widget");

    {
        let mut st = state.borrow_mut();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            // Tab was closed between activation and materialization; drop the
            // freshly built widgets instead of spawning orphan processes.
            return false;
        };
        *tab.panes = panes;
        tab.focused_pane_id = focused_pane_id;
        tab.next_pane_id = next_pane_id;
    }

    let stack_name = tab_root_widget_name(tab_id);
    if term_stack.child_by_name(&stack_name).is_none() {
        term_stack.add_named(&root_widget, Some(&stack_name));
    }
    wire_tab_terminals_with_restore_dismiss(
        state,
        term_stack,
        tab_list,
        window,
        tab_id,
        restore_legend_dismiss,
    );
    spawn_restored_pane_processes(state, &cfg, tab_id, spawns);

    // Refresh tool-version / task discovery for tabs that had it before, matching
    // the eager restore path. Deferred so the panes realize their cwd first.
    let has_discovery = state
        .borrow()
        .find_tab(tab_id)
        .is_some_and(|(_, tab)| tab.discovery_cwd.is_some());
    if has_discovery {
        let state = state.clone();
        let tab_list = tab_list.clone();
        glib::timeout_add_local_once(Duration::from_millis(200), move || {
            crate::sidebar::discover_for_tab(&state, &tab_list, tab_id, false);
        });
    }

    true
}

/// Create a new VTE terminal, add it to the stack, and register it in state.
/// Returns the tab ID.
pub fn create_terminal(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    working_dir: Option<&str>,
    _cwd_label: Option<&gtk::Label>,
) -> u32 {
    let cfg = crate::config::ghostty_config();
    let (terminal, container) = build_terminal(&cfg);

    // Register the tab with pane tree in state
    let pane_id = 0u32;
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(PaneNode::Leaf(build_pane_leaf(
            pane_id, &terminal, &container, None,
        ))),
        focused_pane_id: pane_id,
        next_pane_id: 1,
        close_on_exit: true,
        respawn_on_exit: None,
    });
    let state = runtime.shared_state();

    // Determine spawn argv: use tmux-backed command if the active workspace requests it
    let (tmux_backed, ws_name, host_config_name) = {
        let st = state.borrow();
        let ws = st.active_ws();
        (
            ws.is_some_and(|w| w.tmux_backed) && crate::config::tmux_config().enabled,
            ws.map_or("default".to_string(), |w| w.name.clone()),
            ws.and_then(|w| w.host_config_name.clone()),
        )
    };
    let host_config = host_config_name
        .as_deref()
        .and_then(crate::config::host_config);
    let tmux_session_name = if tmux_backed {
        Some(crate::tmux::session_name(
            &crate::config::tmux_config().session_prefix,
            &ws_name,
            tab_id,
            0,
        ))
    } else {
        None
    };
    let (argv, tmux_backing) = pane_spawn_argv(
        &cfg,
        tmux_backed,
        tmux_session_name.as_deref(),
        working_dir,
        host_config.as_ref(),
    );

    // For tmux-backed panes, cwd is handled by tmux -c flag; pass None to spawn
    let spawn_cwd = if tmux_backed { None } else { working_dir };

    spawn_terminal_process(&terminal, &state, tab_id, pane_id, spawn_cwd, argv);

    // Set tmux_backing on the leaf after spawn
    if let Some(backing) = tmux_backing {
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                leaf.tmux_backing = Some(backing);
            }
        }
    }

    let stack_name = tab_root_widget_name(tab_id);
    term_stack.add_named(&container, Some(&stack_name));
    term_stack.set_visible_child_name(&stack_name);

    tab_id
}

fn create_tmux_terminal_with_callback(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    working_dir: Option<&str>,
    host_config: Option<&crate::host::HostConfig>,
    on_spawn_result: Option<PaneSpawnResultCallback>,
) -> u32 {
    let cfg = crate::config::ghostty_config();
    let (terminal, container) = build_terminal(&cfg);

    let pane_id = 0u32;
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(PaneNode::Leaf(build_pane_leaf(
            pane_id, &terminal, &container, None,
        ))),
        focused_pane_id: pane_id,
        next_pane_id: 1,
        close_on_exit: true,
        respawn_on_exit: None,
    });
    let state = runtime.shared_state();

    let ws_name = {
        let st = state.borrow();
        st.active_ws()
            .map_or("default".to_string(), |w| w.name.clone())
    };
    let tmux_cfg = crate::config::tmux_config();
    let session_name = crate::tmux::session_name(&tmux_cfg.session_prefix, &ws_name, tab_id, 0);
    let (argv, tmux_backing) =
        pane_spawn_argv(&cfg, true, Some(&session_name), working_dir, host_config);

    let on_spawn_result = on_spawn_result.map(|callback| {
        let callback = callback.clone();
        Rc::new(move |result| callback(result, tab_id, pane_id)) as SpawnResultCallback
    });

    let _ = spawn_terminal_process_with_callback(
        &terminal,
        &state,
        tab_id,
        pane_id,
        None,
        argv,
        on_spawn_result,
    );

    if let Some(backing) = tmux_backing {
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                leaf.tmux_backing = Some(backing);
            }
        }
    }

    let stack_name = tab_root_widget_name(tab_id);
    term_stack.add_named(&container, Some(&stack_name));
    term_stack.set_visible_child_name(&stack_name);

    tab_id
}

/// Create and fully wire a visible tmux-backed tab in the UI after tmux
/// availability has already been validated.
pub fn open_explicit_tmux_tab_after_validation(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    name: &str,
    working_dir: Option<&str>,
    host_config: Option<&crate::host::HostConfig>,
) -> u32 {
    let state = runtime.shared_state();
    let state_for_spawn = state.clone();
    let term_stack_for_spawn = term_stack.clone();
    let tab_list_for_spawn = tab_list.clone();
    let target_label = explicit_tmux_target_label(host_config);
    let name_owned = name.to_string();
    let on_spawn_result: PaneSpawnResultCallback = Rc::new(move |result, tab_id, _pane_id| {
        if let Err(err) = result {
            let state = state_for_spawn.clone();
            let term_stack = term_stack_for_spawn.clone();
            let tab_list = tab_list_for_spawn.clone();
            let target_label = target_label.clone();
            let name_owned = name_owned.clone();
            glib::idle_add_local_once(move || {
                crate::show_error_toast(&format!(
                    "Could not start tmux tab \"{}\" on {}: {}",
                    name_owned, target_label, err
                ));
                crate::sidebar::remove_tab_by_id_force(&tab_list, &state, &term_stack, tab_id);
            });
        }
    });

    let tab_id = create_tmux_terminal_with_callback(
        runtime,
        term_stack,
        name,
        working_dir,
        host_config,
        Some(on_spawn_result),
    );
    crate::sidebar::add_tab_row(tab_list, &state, term_stack, tab_id, name, true);
    wire_tab_terminals(&state, term_stack, tab_list, window, tab_id);
    tab_id
}

/// Create a terminal that runs a specific command instead of the user's shell.
/// Used for mise tasks, workspace actions, etc.
pub fn create_terminal_with_command(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    working_dir: Option<&str>,
    argv: Vec<String>,
) -> u32 {
    create_terminal_with_command_mode(runtime, term_stack, name, working_dir, argv, false)
}

/// Start a dedicated command tab without leaving registered state behind when
/// the PTY broker cannot start. Task launches use this checked path so their
/// caller can show one actionable toast instead of presenting a dead tab.
pub(crate) fn create_terminal_with_command_checked(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    working_dir: Option<&str>,
    argv: Vec<String>,
) -> Result<u32, String> {
    let cfg = crate::config::ghostty_config();
    let (terminal, container) = build_terminal(&cfg);
    let launch_command = display_command_for_argv(&argv);
    let pane_id = 0u32;
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(PaneNode::Leaf(build_pane_leaf(
            pane_id,
            &terminal,
            &container,
            launch_command,
        ))),
        focused_pane_id: pane_id,
        next_pane_id: 1,
        close_on_exit: false,
        respawn_on_exit: None,
    });
    let state = runtime.shared_state();
    if let Err(error) = spawn_terminal_process_with_callback(
        &terminal,
        &state,
        tab_id,
        pane_id,
        working_dir,
        argv,
        None,
    ) {
        state.borrow_mut().remove_tab(tab_id);
        return Err(error.to_string());
    }

    let stack_name = tab_root_widget_name(tab_id);
    term_stack.add_named(&container, Some(&stack_name));
    term_stack.set_visible_child_name(&stack_name);
    Ok(tab_id)
}

fn display_command_for_argv(argv: &[String]) -> Option<String> {
    (!argv.is_empty()).then(|| argv.join(" "))
}

fn create_terminal_with_command_mode(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    name: &str,
    working_dir: Option<&str>,
    argv: Vec<String>,
    close_on_exit: bool,
) -> u32 {
    let cfg = crate::config::ghostty_config();
    let (terminal, container) = build_terminal(&cfg);
    let launch_command = display_command_for_argv(&argv);

    let pane_id = 0u32;
    let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
        name: name.to_string(),
        panes: Box::new(PaneNode::Leaf(build_pane_leaf(
            pane_id,
            &terminal,
            &container,
            launch_command,
        ))),
        focused_pane_id: pane_id,
        next_pane_id: 1,
        close_on_exit,
        respawn_on_exit: None,
    });
    let state = runtime.shared_state();

    spawn_terminal_process(&terminal, &state, tab_id, pane_id, working_dir, argv);

    let stack_name = tab_root_widget_name(tab_id);
    term_stack.add_named(&container, Some(&stack_name));
    term_stack.set_visible_child_name(&stack_name);

    tab_id
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub fn duplicate_tab(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    source_tab_id: u32,
) -> Option<u32> {
    let state = runtime.shared_state();
    let (name, ssh_command, working_dir, source_ws_id) = {
        let st = state.borrow();
        let (ws, tab) = st.find_tab(source_tab_id)?;
        let source_ws_id = ws.id;
        let leaf = tab
            .panes
            .leaves()
            .into_iter()
            .find(|leaf| leaf.pane_id == tab.focused_pane_id)
            .or_else(|| tab.panes.leaves().into_iter().next())?;
        let ssh_command = leaf
            .ssh_command()
            .map(|argv| remote_split_respawn(&argv, &leaf.location_state));
        let working_dir = leaf.local_cwd();
        (
            format!("{} copy", tab.name),
            ssh_command,
            working_dir,
            source_ws_id,
        )
    };

    // Ensure the new tab lands in the source tab's workspace, not whatever is currently active
    {
        let mut st = state.borrow_mut();
        let _ = st.activate_workspace(source_ws_id);
    }

    let tab_id = if let Some(argv) = ssh_command {
        create_terminal_with_command_mode(runtime, term_stack, &name, None, argv, true)
    } else {
        create_terminal(runtime, term_stack, &name, working_dir.as_deref(), None)
    };

    crate::sidebar::add_tab_row(tab_list, &state, term_stack, tab_id, &name, true);
    wire_tab_terminals(&state, term_stack, tab_list, window, tab_id);
    Some(tab_id)
}
#[cfg(test)]
mod tests {
    use super::{
        apply_dashboard_poll_results, apply_tmux_kill_result, apply_tmux_kill_result_in_workspace,
        build_editor_argv, build_restored_tab_legend, child_exit_ui_action,
        classify_tmux_kill_result, collect_dashboard_targets, emit_probe_transition_event,
        expand_leading_tilde, extract_host_from_title, extract_path_from_title,
        extract_taarof_branch_from_title, has_exactly_one_file_and_one_url_candidate,
        is_remote_host, is_taarof_activity_termprop, kill_session_by_name_with_hint_using,
        launchable_uri_for_terminal_link, local_selection_drag_moved_enough, local_selection_rects,
        location_metadata_from_terminal_state, looks_like_path_candidate,
        match_terminal_link_candidates, pane_spawn_argv, parse_path_line_col,
        parse_termprop_activity_state, read_git_branch, resolve_attach_target_tab_id_in_workspace,
        resolve_attached_session_pane_in_list, resolve_dispatch_target_ids,
        resolve_existing_bare_path, resolve_file_path, resolve_local_file_match_with_metadata,
        resolve_session_target, resolve_terminal_link_candidates, resolved_file_candidate,
        resolved_target_pane_id, restored_ssh_command, run_tmux_command,
        run_tmux_list_sessions_command_sync_result, saved_cwd_to_path, should_broadcast_commit,
        should_capture_plain_selection_drag, should_record_probe_failure_diagnostic,
        ssh_command_host, terminal_grid_point_at, terminal_hyperlink_confirmation_item,
        terminal_hyperlink_hover_cursor_name, terminal_link_disambiguation_items,
        terminal_match_hover_cursor_name, terminal_text_range_between_points,
        terminal_url_regex_compile_flags, tmux_reports_no_server, tmux_session_exists,
        token_at_column, with_terminal_link_regexes, AttachedSessionPane, ChildExitUiAction,
        DashboardPollOutcome, DispatchTabSnapshot, DispatchTargetId, FileMatch,
        LocalSelectionGeometry, LocalSelectionRect, ResolvedTerminalLink, TerminalGridPoint,
        TerminalLinkActivationGuard, TerminalLinkCandidate, TerminalLinkCandidates,
        TerminalLinkDisambiguationItem, TerminalLinkKind, TerminalTextRange, TmuxKillResult,
        FILE_PATH_REGEX_PATTERN, PCRE2_MULTILINE, TERMINAL_LINK_REGEX_COMPILES, URL_REGEX_PATTERN,
    };
    use crate::config::GhosttyConfig;
    use crate::dashboard::DetachedSession;
    use crate::pane::PaneNode;
    use crate::probe::{ProbeState, ProbeTransition};
    use crate::session::SavedPaneNode;
    use crate::tmux::TmuxTarget;
    use crate::workspace::{AgentActivityState, TabKind};
    use crate::{AppState, BroadcastScope};
    use std::ffi::{c_void, CString};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn detached_poll_snapshot(
        state: &AppState,
    ) -> Vec<(
        crate::dashboard::DetachedSessionCommandKey,
        std::time::Instant,
    )> {
        state
            .detached_sessions
            .iter()
            .map(|session| {
                (
                    crate::dashboard::detached_session_command_key(
                        &session.target,
                        &session.session_name,
                    ),
                    session.detached_at,
                )
            })
            .collect()
    }

    #[test]
    fn remote_host_normalization_is_shared() {
        let local_host: String = glib::host_name().into();
        assert!(!is_remote_host(None));
        assert!(!is_remote_host(Some("")));
        assert!(!is_remote_host(Some("   ")));
        assert!(!is_remote_host(Some("localhost")));
        assert!(!is_remote_host(Some("LOCALHOST")));
        assert!(!is_remote_host(Some(" localhost ")));
        assert!(!is_remote_host(Some(local_host.as_str())));
        assert!(is_remote_host(Some("dev.ts")));
        assert!(is_remote_host(Some(" dev.ts ")));
    }

    #[link(name = "pcre2-8")]
    unsafe extern "C" {
        fn pcre2_compile_8(
            pattern: *const u8,
            length: usize,
            options: u32,
            errorcode: *mut i32,
            erroroffset: *mut usize,
            ccontext: *mut c_void,
        ) -> *mut c_void;
        fn pcre2_code_free_8(code: *mut c_void);
        fn pcre2_match_data_create_from_pattern_8(
            code: *const c_void,
            gcontext: *mut c_void,
        ) -> *mut c_void;
        fn pcre2_match_data_free_8(match_data: *mut c_void);
        fn pcre2_match_8(
            code: *const c_void,
            subject: *const u8,
            length: usize,
            startoffset: usize,
            options: u32,
            match_data: *mut c_void,
            mcontext: *mut c_void,
        ) -> i32;
        fn pcre2_get_ovector_pointer_8(match_data: *mut c_void) -> *mut usize;
    }

    struct Pcre2Code {
        code: *mut c_void,
        match_data: *mut c_void,
    }

    impl Pcre2Code {
        fn compile(pattern: &str) -> Self {
            const PCRE2_ZERO_TERMINATED: usize = usize::MAX;
            let pattern = CString::new(pattern).expect("regex pattern should not contain NUL");
            let mut error_code = 0;
            let mut error_offset = 0;
            let code = unsafe {
                pcre2_compile_8(
                    pattern.as_ptr().cast(),
                    PCRE2_ZERO_TERMINATED,
                    terminal_url_regex_compile_flags(),
                    &mut error_code,
                    &mut error_offset,
                    std::ptr::null_mut(),
                )
            };
            assert!(
                !code.is_null(),
                "PCRE2 failed to compile at offset {error_offset}: error {error_code}"
            );
            let match_data =
                unsafe { pcre2_match_data_create_from_pattern_8(code, std::ptr::null_mut()) };
            assert!(!match_data.is_null(), "PCRE2 match data allocation failed");
            Self { code, match_data }
        }

        fn first_match_spanning(&self, subject: &str, column: usize) -> Option<String> {
            let bytes = subject.as_bytes();
            let mut start_offset = 0;
            while start_offset <= bytes.len() {
                let result = unsafe {
                    pcre2_match_8(
                        self.code,
                        bytes.as_ptr(),
                        bytes.len(),
                        start_offset,
                        0,
                        self.match_data,
                        std::ptr::null_mut(),
                    )
                };
                if result < 0 {
                    return None;
                }
                let ovector = unsafe { pcre2_get_ovector_pointer_8(self.match_data) };
                let start = unsafe { *ovector };
                let end = unsafe { *ovector.add(1) };
                if start <= column && column < end {
                    return Some(subject[start..end].to_string());
                }
                start_offset = start.saturating_add(1);
            }
            None
        }
    }

    impl Drop for Pcre2Code {
        fn drop(&mut self) {
            unsafe {
                pcre2_match_data_free_8(self.match_data);
                pcre2_code_free_8(self.code);
            }
        }
    }

    fn first_registered_link_match_at(row: &str, column: usize) -> Option<(&'static str, String)> {
        [
            ("url", URL_REGEX_PATTERN),
            ("file", FILE_PATH_REGEX_PATTERN),
        ]
        .into_iter()
        .find_map(|(kind, pattern)| {
            Pcre2Code::compile(pattern)
                .first_match_spanning(row, column)
                .map(|matched| (kind, matched))
        })
    }

    fn leaf(cwd: Option<&str>, ssh_command: Option<Vec<&str>>) -> SavedPaneNode {
        SavedPaneNode::Leaf {
            work_origin: None,
            cwd: cwd.map(str::to_string),
            ssh_command: ssh_command.map(|argv| argv.into_iter().map(str::to_string).collect()),
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session: None,
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn test_child_exit_ui_action_closes_last_window() {
        assert_eq!(
            child_exit_ui_action(1, true, 1),
            ChildExitUiAction::CloseWindow
        );
    }

    #[test]
    fn test_child_exit_ui_action_closes_tab_when_other_tabs_exist() {
        assert_eq!(
            child_exit_ui_action(1, true, 2),
            ChildExitUiAction::CloseTab
        );
    }

    #[test]
    fn test_child_exit_ui_action_closes_only_the_exiting_pane() {
        assert_eq!(
            child_exit_ui_action(2, true, 1),
            ChildExitUiAction::ClosePane
        );
    }

    #[test]
    fn test_child_exit_ui_action_preserves_shell_when_close_on_exit_disabled() {
        assert_eq!(child_exit_ui_action(1, false, 1), ChildExitUiAction::None);
    }

    #[test]
    fn probe_transition_events_are_recorded_for_degraded_changes() {
        let mut state = AppState::new();
        let transition = ProbeTransition {
            previous: ProbeState::Ok,
            current: ProbeState::Stale,
        };

        emit_probe_transition_event(
            &mut state,
            "tmux-pane-info",
            serde_json::json!({ "tab_id": 7, "pane_id": 3 }),
            &transition,
            Some("timeout"),
        );

        let event = state
            .event_store
            .entries()
            .into_iter()
            .last()
            .expect("probe transition event should be recorded");
        assert_eq!(event.event_type, "probe_state_changed");
        assert_eq!(event.payload["probe"], serde_json::json!("tmux-pane-info"));
        assert_eq!(event.payload["previous_state"], serde_json::json!("ok"));
        assert_eq!(event.payload["state"], serde_json::json!("stale"));
        assert_eq!(event.payload["error"], serde_json::json!("timeout"));
    }

    #[test]
    fn probe_transition_events_ignore_non_degraded_changes() {
        let mut state = AppState::new();
        let baseline = state.event_store.len();
        let transition = ProbeTransition {
            previous: ProbeState::Unknown,
            current: ProbeState::Ok,
        };

        emit_probe_transition_event(
            &mut state,
            "host-status",
            serde_json::json!({ "workspace_id": 1 }),
            &transition,
            None,
        );

        assert_eq!(state.event_store.len(), baseline);
    }

    #[test]
    fn probe_transition_degraded_to_degraded_changes_do_not_emit_events() {
        let mut state = AppState::new();
        let baseline = state.event_store.len();
        let transition = ProbeTransition {
            previous: ProbeState::Error,
            current: ProbeState::Stale,
        };

        emit_probe_transition_event(
            &mut state,
            "dashboard-state",
            serde_json::json!({ "dashboard_targets": 2 }),
            &transition,
            Some("still failing"),
        );

        assert_eq!(state.event_store.len(), baseline);
    }

    #[test]
    fn probe_failure_diagnostics_only_record_when_entering_degraded_state() {
        assert!(should_record_probe_failure_diagnostic(&ProbeTransition {
            previous: ProbeState::Ok,
            current: ProbeState::Stale,
        }));
        assert!(!should_record_probe_failure_diagnostic(&ProbeTransition {
            previous: ProbeState::Stale,
            current: ProbeState::Stale,
        }));
        assert!(!should_record_probe_failure_diagnostic(&ProbeTransition {
            previous: ProbeState::Error,
            current: ProbeState::Stale,
        }));
    }

    #[test]
    fn dashboard_tmux_probe_recognizes_standard_no_server_message() {
        assert!(tmux_reports_no_server(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(tmux_reports_no_server(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
        assert!(!tmux_reports_no_server("permission denied"));
    }

    #[test]
    fn dashboard_list_sessions_probe_treats_no_server_as_empty() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'no server running on /tmp/tmux-1000/default' >&2; exit 1".to_string(),
        ];

        let output = run_tmux_list_sessions_command_sync_result(&argv)
            .expect("no-server tmux probe should be treated as empty");

        assert!(output.is_empty());
    }

    #[test]
    fn dashboard_list_sessions_probe_keeps_real_failures() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'permission denied' >&2; exit 1".to_string(),
        ];

        let error = run_tmux_list_sessions_command_sync_result(&argv)
            .expect_err("non-no-server tmux probe failures should remain errors");

        assert!(error.contains("permission denied"));
    }

    #[test]
    fn dashboard_poll_first_load_with_no_sessions_marks_probe_ok() {
        let mut state = AppState::new();
        let poll_context = state.begin_dashboard_poll(&[]);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &[],
                live_sessions: &[],
                detached_probe_errors: &[],
                dashboard_errors: &[],
                dashboard_target_count: 0,
                detached_target_count: 0,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.dashboard_state.probe.state, ProbeState::Ok);
        assert!(state.dashboard_state.sessions.is_empty());
        assert!(state.dashboard_state.hosts.is_empty());
        assert_eq!(state.dashboard_state.probe.error, None);
    }

    #[test]
    fn dashboard_poll_prunes_only_authoritatively_missing_targets() {
        let local_name = "taarof--local--gone";
        let remote_name = "taarof--remote--unknown";
        let remote_target = TmuxTarget::Remote {
            ssh_target: "builder".to_string(),
        };
        let mut state = AppState::new();
        state.detached_sessions = vec![
            DetachedSession {
                session_name: local_name.to_string(),
                host: "localhost".to_string(),
                workspace: "local".to_string(),
                target: TmuxTarget::Local,
                detached_at: Instant::now(),
                last_command: Some("cargo test".to_string()),
                finished: false,
            },
            DetachedSession {
                session_name: remote_name.to_string(),
                host: "builder".to_string(),
                workspace: "remote".to_string(),
                target: remote_target.clone(),
                detached_at: Instant::now(),
                last_command: Some("cargo test".to_string()),
                finished: false,
            },
        ];
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &state.detached_sessions,
            &[],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let local_key =
            crate::dashboard::detached_session_command_key(&TmuxTarget::Local, local_name);
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local, remote_target.clone()]);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[(TmuxTarget::Local, vec![])],
                detached_probe_errors: &[(
                    local_key,
                    "detached session probe failed: no server running".to_string(),
                )],
                dashboard_errors: &[(
                    Some(remote_target.clone()),
                    "dashboard tmux probe for builder failed: ssh timeout".to_string(),
                )],
                dashboard_target_count: 2,
                detached_target_count: 2,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.detached_sessions.len(), 1);
        assert!(state.detached_sessions[0].matches_target(remote_name, &remote_target));
        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, remote_name);
        assert_eq!(state.dashboard_state.sessions[0].target, remote_target);
        assert!(state.dashboard_state.probe.state.is_degraded());
        assert_eq!(
            state.dashboard_state.probe.error.as_deref(),
            Some(
                "dashboard probe incomplete: dashboard tmux probe for builder failed: ssh timeout"
            )
        );
        let local_host = state
            .dashboard_state
            .hosts
            .iter()
            .find(|host| host.name == "localhost")
            .expect("the successful empty local snapshot should retain a zero-count host");
        assert_eq!(local_host.session_count, 0);
        let remote_host = state
            .dashboard_state
            .hosts
            .iter()
            .find(|host| host.name == "builder")
            .expect("the failed remote target should retain its last-known host");
        assert_eq!(remote_host.session_count, 1);
    }

    #[test]
    fn dashboard_poll_prunes_session_missing_from_successful_target_snapshot() {
        let missing_name = "taarof--local--missing";
        let present_name = "taarof--local--present";
        let mut state = AppState::new();
        state.detached_sessions = [missing_name, present_name]
            .into_iter()
            .map(|session_name| DetachedSession {
                session_name: session_name.to_string(),
                host: "localhost".to_string(),
                workspace: "local".to_string(),
                target: TmuxTarget::Local,
                detached_at: Instant::now(),
                last_command: Some("cargo test".to_string()),
                finished: false,
            })
            .collect();
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &state.detached_sessions,
            &[],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let missing_key =
            crate::dashboard::detached_session_command_key(&TmuxTarget::Local, missing_name);
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local]);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[(
                    TmuxTarget::Local,
                    vec![(present_name.to_string(), 1, false, 1)],
                )],
                detached_probe_errors: &[(
                    missing_key,
                    "detached session probe failed: session not found".to_string(),
                )],
                dashboard_errors: &[],
                dashboard_target_count: 1,
                detached_target_count: 1,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.detached_sessions.len(), 1);
        assert!(state.detached_sessions[0].matches_target(present_name, &TmuxTarget::Local));
        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, present_name);
        assert!(state.dashboard_state.sessions[0].is_detached);
        assert_eq!(state.dashboard_state.probe.state, ProbeState::Ok);
        assert_eq!(state.dashboard_state.hosts.len(), 1);
        assert_eq!(state.dashboard_state.hosts[0].session_count, 1);
    }

    #[test]
    fn dashboard_poll_preserves_detached_session_on_transient_remote_failure() {
        let remote_name = "taarof--remote--unknown";
        let remote_target = TmuxTarget::Remote {
            ssh_target: "builder".to_string(),
        };
        let mut state = AppState::new();
        state.detached_sessions = vec![DetachedSession {
            session_name: remote_name.to_string(),
            host: "builder".to_string(),
            workspace: "remote".to_string(),
            target: remote_target.clone(),
            detached_at: Instant::now(),
            last_command: Some("cargo test".to_string()),
            finished: false,
        }];
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &state.detached_sessions,
            &[],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let remote_key =
            crate::dashboard::detached_session_command_key(&remote_target, remote_name);
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let poll_context = state.begin_dashboard_poll(std::slice::from_ref(&remote_target));

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[],
                detached_probe_errors: &[(
                    remote_key,
                    "detached session probe for remote failed: ssh timeout".to_string(),
                )],
                dashboard_errors: &[(
                    Some(remote_target.clone()),
                    "dashboard tmux probe for builder failed: ssh timeout".to_string(),
                )],
                dashboard_target_count: 1,
                detached_target_count: 1,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.detached_sessions.len(), 1);
        assert!(state.detached_sessions[0].matches_target(remote_name, &remote_target));
        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, remote_name);
        assert!(state.dashboard_state.probe.state.is_degraded());
    }

    #[test]
    fn dashboard_poll_does_not_prune_session_detached_after_poll_started() {
        let session_name = "taarof--local--same-identity";
        let captured_at = Instant::now();
        let mut state = AppState::new();
        state.detached_sessions = vec![DetachedSession {
            session_name: session_name.to_string(),
            host: "localhost".to_string(),
            workspace: "local".to_string(),
            target: TmuxTarget::Local,
            detached_at: captured_at,
            last_command: None,
            finished: false,
        }];
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local]);
        let captured_key =
            crate::dashboard::detached_session_command_key(&TmuxTarget::Local, session_name);
        let current_commands =
            std::collections::HashMap::from([(captured_key.clone(), "bash".to_string())]);

        // Simulate reattach + detach of the same tmux identity after the
        // background poll captured its generation.
        state.detached_sessions = vec![DetachedSession {
            session_name: session_name.to_string(),
            host: "localhost".to_string(),
            workspace: "local".to_string(),
            target: TmuxTarget::Local,
            detached_at: captured_at + std::time::Duration::from_secs(1),
            last_command: Some("cargo test".to_string()),
            finished: false,
        }];

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &current_commands,
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[(TmuxTarget::Local, vec![])],
                detached_probe_errors: &[(
                    captured_key,
                    "detached session probe failed: no server running".to_string(),
                )],
                dashboard_errors: &[],
                dashboard_target_count: 1,
                detached_target_count: 1,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.detached_sessions.len(), 1);
        assert!(state.detached_sessions[0].matches_target(session_name, &TmuxTarget::Local));
        assert_eq!(
            state.detached_sessions[0].detached_at,
            captured_at + std::time::Duration::from_secs(1)
        );
        assert!(!state.detached_sessions[0].finished);
        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, session_name);
        assert_eq!(state.dashboard_state.probe.state, ProbeState::Ok);
        assert_eq!(state.dashboard_state.hosts.len(), 1);
        assert_eq!(state.dashboard_state.hosts[0].session_count, 1);
    }

    #[test]
    fn dashboard_poll_does_not_prune_from_malformed_target_output() {
        let session_name = "taarof--local--preserved";
        let mut state = AppState::new();
        state.detached_sessions = vec![DetachedSession {
            session_name: session_name.to_string(),
            host: "localhost".to_string(),
            workspace: "local".to_string(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo test".to_string()),
            finished: false,
        }];
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &state.detached_sessions,
            &[],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let key = crate::dashboard::detached_session_command_key(&TmuxTarget::Local, session_name);
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local]);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[],
                detached_probe_errors: &[(
                    key,
                    "detached session probe failed: invalid response".to_string(),
                )],
                dashboard_errors: &[(
                    Some(TmuxTarget::Local),
                    "dashboard tmux probe for localhost returned invalid output: malformed tmux list-sessions output at line 2".to_string(),
                )],
                dashboard_target_count: 1,
                detached_target_count: 1,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert_eq!(state.detached_sessions.len(), 1);
        assert!(state.detached_sessions[0].matches_target(session_name, &TmuxTarget::Local));
        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, session_name);
        assert_eq!(state.dashboard_state.hosts.len(), 1);
        assert_eq!(state.dashboard_state.hosts[0].session_count, 1);
        assert!(state.dashboard_state.probe.state.is_degraded());
    }

    #[test]
    fn dashboard_poll_does_not_restore_session_killed_during_poll() {
        let local_name = "taarof--local--killed";
        let remote_target = TmuxTarget::Remote {
            ssh_target: "builder".to_string(),
        };
        let mut state = AppState::new();
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &[],
            &[
                (
                    TmuxTarget::Local,
                    vec![(local_name.to_string(), 1, false, 1)],
                ),
                (
                    remote_target.clone(),
                    vec![("remote-old".to_string(), 1, false, 1)],
                ),
            ],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local, remote_target.clone()]);

        state.invalidate_dashboard_session_snapshot(&TmuxTarget::Local, local_name);
        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &[],
                live_sessions: &[
                    (
                        TmuxTarget::Local,
                        vec![(local_name.to_string(), 1, false, 1)],
                    ),
                    (
                        remote_target.clone(),
                        vec![("remote-new".to_string(), 2, false, 1)],
                    ),
                ],
                detached_probe_errors: &[],
                dashboard_errors: &[],
                dashboard_target_count: 2,
                detached_target_count: 0,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert!(!state
            .dashboard_state
            .sessions
            .iter()
            .any(|session| session.target == TmuxTarget::Local));
        assert!(state
            .dashboard_state
            .sessions
            .iter()
            .any(|session| session.target == remote_target && session.name == "remote-new"));
        let local_host = state
            .dashboard_state
            .hosts
            .iter()
            .find(|host| host.name == "localhost")
            .expect("kill tombstone should retain the known local host");
        assert_eq!(local_host.session_count, 0);
    }

    #[test]
    fn dashboard_poll_does_not_restore_session_attached_during_poll() {
        let session_name = "taarof--local--attached";
        let mut state = AppState::new();
        state.detached_sessions = vec![DetachedSession {
            session_name: session_name.to_string(),
            host: "localhost".to_string(),
            workspace: "local".to_string(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo test".to_string()),
            finished: false,
        }];
        state.dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &state.detached_sessions,
            &[(
                TmuxTarget::Local,
                vec![(session_name.to_string(), 1, false, 1)],
            )],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let polled_detached_sessions = detached_poll_snapshot(&state);
        let poll_context = state.begin_dashboard_poll(&[TmuxTarget::Local]);

        state.detached_sessions.clear();
        state.invalidate_dashboard_session_snapshot(&TmuxTarget::Local, session_name);
        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &polled_detached_sessions,
                live_sessions: &[(
                    TmuxTarget::Local,
                    vec![(session_name.to_string(), 1, false, 1)],
                )],
                detached_probe_errors: &[],
                dashboard_errors: &[],
                dashboard_target_count: 1,
                detached_target_count: 1,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert!(state.detached_sessions.is_empty());
        assert!(state.dashboard_state.sessions.is_empty());
        assert_eq!(state.dashboard_state.hosts.len(), 1);
        assert_eq!(state.dashboard_state.hosts[0].session_count, 0);
    }

    #[test]
    fn older_dashboard_poll_cannot_overwrite_newer_target_result() {
        let mut state = AppState::new();
        let older_poll = state.begin_dashboard_poll(&[TmuxTarget::Local]);
        let newer_poll = state.begin_dashboard_poll(&[TmuxTarget::Local]);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &[],
                live_sessions: &[(TmuxTarget::Local, vec![("newer".to_string(), 2, false, 1)])],
                detached_probe_errors: &[],
                dashboard_errors: &[],
                dashboard_target_count: 1,
                detached_target_count: 0,
                session_prefix: "taarof",
                poll_context: &newer_poll,
            },
        );
        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &[],
                live_sessions: &[(TmuxTarget::Local, vec![("older".to_string(), 1, false, 1)])],
                detached_probe_errors: &[],
                dashboard_errors: &[],
                dashboard_target_count: 1,
                detached_target_count: 0,
                session_prefix: "taarof",
                poll_context: &older_poll,
            },
        );

        assert_eq!(state.dashboard_state.sessions.len(), 1);
        assert_eq!(state.dashboard_state.sessions[0].name, "newer");
        assert_eq!(state.dashboard_state.hosts.len(), 1);
        assert_eq!(state.dashboard_state.hosts[0].session_count, 1);
        assert_eq!(state.dashboard_state.probe.state, ProbeState::Ok);
    }

    #[test]
    fn dashboard_probe_skips_missing_local_tmux_when_unused() {
        let state = AppState::new();
        let app_config = crate::config::AppConfig::default();

        let targets = collect_dashboard_targets(&state, &app_config);

        assert!(
            targets.is_empty(),
            "local tmux should not be probed when no runtime feature depends on it"
        );
    }

    #[test]
    fn dashboard_probe_reports_real_tmux_failure_when_required() {
        let mut state = AppState::new();
        state
            .active_ws_mut()
            .expect("default workspace should exist")
            .tmux_backed = true;
        let app_config = crate::config::AppConfig::default();

        let targets = collect_dashboard_targets(&state, &app_config);
        assert_eq!(targets, vec![crate::tmux::TmuxTarget::Local]);
        let poll_context = state.begin_dashboard_poll(&targets);

        apply_dashboard_poll_results(
            &mut state,
            DashboardPollOutcome {
                current_commands: &std::collections::HashMap::new(),
                polled_detached_sessions: &[],
                live_sessions: &[],
                detached_probe_errors: &[],
                dashboard_errors: &[(
                    Some(TmuxTarget::Local),
                    "dashboard tmux probe for localhost failed: permission denied".to_string(),
                )],
                dashboard_target_count: targets.len(),
                detached_target_count: 0,
                session_prefix: "taarof",
                poll_context: &poll_context,
            },
        );

        assert!(state.dashboard_state.probe.state.is_degraded());
        assert_eq!(
            state.dashboard_state.probe.error.as_deref(),
            Some("dashboard probe incomplete: dashboard tmux probe for localhost failed: permission denied")
        );
    }

    #[test]
    fn test_saved_cwd_to_path_accepts_plain_absolute_paths() {
        assert_eq!(
            saved_cwd_to_path(Some("/tmp/user/project")),
            Some("/tmp/user/project".to_string())
        );
    }

    #[test]
    fn test_saved_cwd_to_path_decodes_file_uri() {
        assert_eq!(
            saved_cwd_to_path(Some("file://host/tmp/user/project")),
            Some("/tmp/user/project".to_string())
        );
    }

    #[test]
    fn test_saved_cwd_to_path_strips_legacy_host_prefix() {
        assert_eq!(
            saved_cwd_to_path(Some("host/tmp/user/project")),
            Some("/tmp/user/project".to_string())
        );
    }

    #[test]
    fn test_plain_terminal_links_only_allow_http_and_https() {
        assert_eq!(
            launchable_uri_for_terminal_link(
                "HTTPS://github.com/example/taarof",
                TerminalLinkKind::PlainUrl,
            ),
            Some("HTTPS://github.com/example/taarof".to_string())
        );
        assert_eq!(
            launchable_uri_for_terminal_link("mailto:test@example.com", TerminalLinkKind::PlainUrl),
            None
        );
    }

    #[derive(Debug, serde::Deserialize)]
    struct TerminalLinksFixture {
        version: u32,
        cases: Vec<TerminalLinksFixtureCase>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct TerminalLinksFixtureCase {
        token: String,
        ambiguous: bool,
        candidates: Vec<TerminalLinkCandidate>,
    }

    #[test]
    fn terminal_link_matcher_matches_shared_fixture() {
        let fixture: TerminalLinksFixture =
            serde_json::from_str(include_str!("../../fixtures/terminal-links.json"))
                .expect("terminal link fixture should parse");
        assert_eq!(fixture.version, 1);

        for case in fixture.cases {
            let expected = TerminalLinkCandidates {
                ambiguous: case.ambiguous,
                candidates: case.candidates,
            };
            if expected.ambiguous {
                assert!(
                    has_exactly_one_file_and_one_url_candidate(&expected.candidates),
                    "ambiguous fixture case should have exactly one file and one url candidate: {:?}",
                    case.token
                );
            }
            assert_eq!(
                match_terminal_link_candidates(&case.token),
                expected,
                "terminal link fixture mismatch for {:?}",
                case.token
            );
        }
    }

    #[test]
    fn terminal_link_matcher_does_not_require_files_to_exist() {
        assert_eq!(
            match_terminal_link_candidates("definitely-missing-9f3a.sh"),
            TerminalLinkCandidates {
                ambiguous: true,
                candidates: vec![
                    TerminalLinkCandidate::File {
                        path: "definitely-missing-9f3a.sh".to_string(),
                        line: None,
                        col: None,
                    },
                    TerminalLinkCandidate::Url {
                        url: "https://definitely-missing-9f3a.sh".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn terminal_link_matcher_keeps_path_line_domain_port_ambiguity() {
        assert_eq!(
            match_terminal_link_candidates("terminal.rs:112"),
            TerminalLinkCandidates {
                ambiguous: true,
                candidates: vec![
                    TerminalLinkCandidate::File {
                        path: "terminal.rs".to_string(),
                        line: Some(112),
                        col: None,
                    },
                    TerminalLinkCandidate::Url {
                        url: "https://terminal.rs:112".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn terminal_link_matcher_preserves_explicit_url_balanced_trailing_paren() {
        let url = "https://en.wikipedia.org/wiki/Ruby_(programming_language)";
        assert_eq!(
            match_terminal_link_candidates(url),
            TerminalLinkCandidates {
                ambiguous: false,
                candidates: vec![TerminalLinkCandidate::Url {
                    url: url.to_string(),
                }],
            }
        );
    }

    #[test]
    fn terminal_link_regex_add_order_prefers_full_file_path_over_schemeless_suffix() {
        let row = "a/b/c.rs:1";
        let clicked_column = row.find("c.rs").expect("test row should contain suffix") + 1;

        assert_eq!(
            first_registered_link_match_at(row, clicked_column),
            Some(("file", "a/b/c.rs:1".to_string()))
        );
    }

    #[test]
    fn terminal_link_regex_resolves_exact_leading_tilde_path_with_location() {
        let row = "open ~/.config/taarof/config.toml:12:3 now";
        let clicked_column = row
            .find("config.toml")
            .expect("test row should contain filename")
            + 2;
        let matched = first_registered_link_match_at(row, clicked_column);

        assert_eq!(
            matched,
            Some(("file", "~/.config/taarof/config.toml:12:3".to_string())),
            "the VTE matcher must retain the leading tilde instead of truncating to a root path",
        );

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp_dir("taarof-link-tilde-home");
        let file = home.join(".config").join("taarof").join("config.toml");
        std::fs::create_dir_all(file.parent().expect("file has parent")).expect("create parents");
        std::fs::write(&file, b"x").expect("write tilde-path file");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let match_text = matched.expect("leading-tilde path should match").1;
        let candidates = match_terminal_link_candidates(&match_text);
        assert_eq!(
            resolve_terminal_link_candidates(&candidates, None, None),
            Some(ResolvedTerminalLink::File {
                path: file,
                line: 12,
                col: Some(3),
            }),
        );

        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn terminal_link_regex_respects_tilde_and_path_token_boundaries() {
        for (row, clicked_text, expected_match) in [
            ("example.com/", "example", "example.com"),
            ("localhost:8080/", "localhost", "localhost:8080"),
            (
                "https://example.com/~user/notes.md",
                "~user",
                "https://example.com/~user/notes.md",
            ),
        ] {
            assert_eq!(
                first_registered_link_match_at(
                    row,
                    row.find(clicked_text).expect("clicked text should exist") + 1,
                ),
                Some(("url", expected_match.to_string())),
                "the URL matcher must preserve prior behavior for {row:?}",
            );
        }

        for (row, clicked_column) in [("~user/notes.md", 2), ("prefix~/notes.md", 9)] {
            assert_eq!(
                first_registered_link_match_at(row, clicked_column),
                None,
                "{row:?} must not produce a truncated partial file match",
            );
        }
    }

    #[test]
    fn terminal_link_regex_keeps_tilde_shapes_and_trims_sentence_punctuation_downstream() {
        let cases = [
            (
                "open ~/notes.md now",
                "notes.md",
                "~/notes.md",
                TerminalLinkCandidate::File {
                    path: "~/notes.md".to_string(),
                    line: None,
                    col: None,
                },
            ),
            (
                "saved ~/notes.md., then stopped",
                "notes.md",
                "~/notes.md.",
                TerminalLinkCandidate::File {
                    path: "~/notes.md".to_string(),
                    line: None,
                    col: None,
                },
            ),
            (
                "open (~/.config/x.toml:12:3).",
                "x.toml",
                "~/.config/x.toml:12:3",
                TerminalLinkCandidate::File {
                    path: "~/.config/x.toml".to_string(),
                    line: Some(12),
                    col: Some(3),
                },
            ),
            (
                "open ~/.config/x.toml:12.",
                "x.toml",
                "~/.config/x.toml:12",
                TerminalLinkCandidate::File {
                    path: "~/.config/x.toml".to_string(),
                    line: Some(12),
                    col: None,
                },
            ),
            (
                "open ~/.config/x.toml:12:3.",
                "x.toml",
                "~/.config/x.toml:12:3",
                TerminalLinkCandidate::File {
                    path: "~/.config/x.toml".to_string(),
                    line: Some(12),
                    col: Some(3),
                },
            ),
            (
                "edit ~/.bashrc now",
                ".bashrc",
                "~/.bashrc",
                TerminalLinkCandidate::File {
                    path: "~/.bashrc".to_string(),
                    line: None,
                    col: None,
                },
            ),
            (
                "open ~/notes now",
                "notes",
                "~/notes",
                TerminalLinkCandidate::File {
                    path: "~/notes".to_string(),
                    line: None,
                    col: None,
                },
            ),
            (
                "existing src/main.rs.",
                "main.rs",
                "src/main.rs.",
                TerminalLinkCandidate::File {
                    path: "src/main.rs".to_string(),
                    line: None,
                    col: None,
                },
            ),
            (
                "see src/main.rs:12.",
                "main.rs",
                "src/main.rs:12",
                TerminalLinkCandidate::File {
                    path: "src/main.rs".to_string(),
                    line: Some(12),
                    col: None,
                },
            ),
            (
                "see src/main.rs:12:3.",
                "main.rs",
                "src/main.rs:12:3",
                TerminalLinkCandidate::File {
                    path: "src/main.rs".to_string(),
                    line: Some(12),
                    col: Some(3),
                },
            ),
        ];

        for (row, clicked_text, expected_match, expected_candidate) in cases {
            let clicked_column = row.find(clicked_text).expect("clicked text should exist") + 1;
            let matched = first_registered_link_match_at(row, clicked_column);
            assert_eq!(
                matched,
                Some(("file", expected_match.to_string())),
                "unexpected exact PCRE match for {row:?}",
            );
            assert_eq!(
                match_terminal_link_candidates(expected_match),
                TerminalLinkCandidates {
                    ambiguous: false,
                    candidates: vec![expected_candidate],
                },
                "downstream classification should trim only sentence punctuation for {row:?}",
            );
        }
    }

    #[test]
    fn production_resolver_reports_local_existing_file_first() {
        let dir = unique_temp_dir("taarof-link-resolver-disambiguate");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("github.com").join("example").join("taarof");
        std::fs::create_dir_all(file.parent().expect("file has parent")).expect("create parents");
        std::fs::write(&file, b"x").expect("write ambiguous file");
        let cwd = dir.to_str().expect("temp dir path should be utf-8");
        let candidates = match_terminal_link_candidates("github.com/example/taarof");

        assert_eq!(
            resolve_terminal_link_candidates(&candidates, Some(cwd), None),
            Some(ResolvedTerminalLink::NeedsDisambiguation {
                candidates: TerminalLinkCandidates {
                    ambiguous: true,
                    candidates: vec![
                        TerminalLinkCandidate::File {
                            path: "github.com/example/taarof".to_string(),
                            line: None,
                            col: None,
                        },
                        TerminalLinkCandidate::Url {
                            url: "https://github.com/example/taarof".to_string(),
                        },
                    ],
                },
                existing: Some(file),
            })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn production_resolver_keeps_missing_file_first_tokens_inert() {
        let dir = unique_temp_dir("taarof-link-resolver-disambiguate-missing");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cwd = dir.to_str().expect("temp dir path should be utf-8");

        for token in [
            "build.sh",
            "README.md",
            "main.rs",
            "archive.zip",
            "README.md:12",
        ] {
            let candidates = match_terminal_link_candidates(token);

            assert_eq!(
                resolve_terminal_link_candidates(&candidates, Some(cwd), None),
                None,
                "{token} should stay inert when the local file is missing"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn production_resolver_reports_existing_file_as_disambiguation() {
        let dir = unique_temp_dir("taarof-link-resolver-disambiguate-existing-file");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("build.sh");
        std::fs::write(&file, b"#!/bin/sh\n").expect("write build.sh");
        let cwd = dir.to_str().expect("temp dir path should be utf-8");
        let candidates = match_terminal_link_candidates("build.sh");

        assert_eq!(
            resolve_terminal_link_candidates(&candidates, Some(cwd), None),
            Some(ResolvedTerminalLink::NeedsDisambiguation {
                candidates: TerminalLinkCandidates {
                    ambiguous: true,
                    candidates: vec![
                        TerminalLinkCandidate::File {
                            path: "build.sh".to_string(),
                            line: None,
                            col: None,
                        },
                        TerminalLinkCandidate::Url {
                            url: "https://build.sh".to_string(),
                        },
                    ],
                },
                existing: Some(file),
            })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn production_resolver_reports_remote_file_first_ambiguity() {
        for (token, expected_url) in [
            ("build.sh", "https://build.sh"),
            ("README.md", "https://README.md"),
            ("main.rs", "https://main.rs"),
            ("archive.zip", "https://archive.zip"),
            ("README.md:12", "https://README.md:12"),
        ] {
            let candidates = match_terminal_link_candidates(token);
            let resolved = resolve_terminal_link_candidates(
                &candidates,
                Some("/remote/project"),
                Some("dev.ts"),
            );

            assert!(
                !matches!(resolved, Some(ResolvedTerminalLink::Url(_))),
                "{token} must not auto-open {expected_url} on a remote pane"
            );
            assert_eq!(
                resolved,
                Some(ResolvedTerminalLink::NeedsDisambiguation {
                    candidates,
                    existing: None,
                }),
                "{token} should ask before opening {expected_url} on a remote pane"
            );
        }
    }

    #[test]
    fn production_resolver_reports_remote_file_first_ambiguity_without_tld_assumptions() {
        let candidates = TerminalLinkCandidates {
            ambiguous: true,
            candidates: vec![
                TerminalLinkCandidate::File {
                    path: "artifact.unknown".to_string(),
                    line: None,
                    col: None,
                },
                TerminalLinkCandidate::Url {
                    url: "https://artifact.unknown".to_string(),
                },
            ],
        };

        assert_eq!(
            resolve_terminal_link_candidates(&candidates, Some("/remote/project"), Some("dev.ts"),),
            Some(ResolvedTerminalLink::NeedsDisambiguation {
                candidates,
                existing: None,
            })
        );
    }

    #[test]
    fn production_resolver_opens_remote_url_first_tokens_directly() {
        for (token, expected) in [
            (
                "github.com/example/taarof",
                "https://github.com/example/taarof",
            ),
            ("docs.rs/serde", "https://docs.rs/serde"),
            ("apps.example.com", "https://apps.example.com"),
            (
                "https://github.com/example/taarof",
                "https://github.com/example/taarof",
            ),
        ] {
            let candidates = match_terminal_link_candidates(token);

            assert_eq!(
                resolve_terminal_link_candidates(
                    &candidates,
                    Some("/remote/project"),
                    Some("dev.ts"),
                ),
                Some(ResolvedTerminalLink::Url(expected.to_string())),
                "{token} should still open directly on a remote pane"
            );
        }
    }

    #[test]
    fn production_resolver_keeps_unambiguous_tokens_direct() {
        let dir = unique_temp_dir("taarof-link-resolver-disambiguate-unambiguous");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cwd = dir.to_str().expect("temp dir path should be utf-8");

        for (token, expected) in [
            ("localhost:3000", "http://localhost:3000"),
            ("apps.example.com", "https://apps.example.com"),
        ] {
            let candidates = match_terminal_link_candidates(token);
            assert_eq!(
                resolve_terminal_link_candidates(&candidates, Some(cwd), None),
                Some(ResolvedTerminalLink::Url(expected.to_string()))
            );
        }

        let candidates = match_terminal_link_candidates("foo.ts");
        assert!(
            !matches!(
                resolve_terminal_link_candidates(&candidates, Some(cwd), None),
                Some(ResolvedTerminalLink::NeedsDisambiguation { .. })
            ),
            "foo.ts must not trigger the disambiguation popover"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn production_resolver_opens_missing_url_first_token_directly() {
        let dir = unique_temp_dir("taarof-link-resolver-disambiguate-docs-rs");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cwd = dir.to_str().expect("temp dir path should be utf-8");
        let candidates = match_terminal_link_candidates("docs.rs/serde");

        assert_eq!(
            resolve_terminal_link_candidates(&candidates, Some(cwd), None),
            Some(ResolvedTerminalLink::Url(
                "https://docs.rs/serde".to_string()
            ))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn terminal_link_disambiguation_items_show_full_destinations_and_existing_file_only() {
        let existing = std::path::PathBuf::from("/tmp/my_repo/a__b/build_file.sh");
        let candidates = TerminalLinkCandidates {
            ambiguous: true,
            candidates: vec![
                TerminalLinkCandidate::File {
                    path: "build_file.sh".to_string(),
                    line: Some(12),
                    col: Some(3),
                },
                TerminalLinkCandidate::Url {
                    url: "https://github.com/kombiz/my_repo_x".to_string(),
                },
            ],
        };

        assert_eq!(
            terminal_link_disambiguation_items(&candidates, Some(&existing)),
            vec![
                TerminalLinkDisambiguationItem::File {
                    label: "Open /tmp/my__repo/a____b/build__file.sh".to_string(),
                    path: existing,
                    line: 12,
                    col: Some(3),
                },
                TerminalLinkDisambiguationItem::Url {
                    label: "Open https://github.com/kombiz/my__repo__x".to_string(),
                    url: "https://github.com/kombiz/my_repo_x".to_string(),
                },
            ]
        );

        assert_eq!(
            terminal_link_disambiguation_items(&candidates, None),
            vec![TerminalLinkDisambiguationItem::Url {
                label: "Open https://github.com/kombiz/my__repo__x".to_string(),
                url: "https://github.com/kombiz/my_repo_x".to_string(),
            }]
        );
    }

    #[test]
    fn terminal_link_resolver_remote_file_first_disambiguation_items_offer_url_only() {
        let candidates = match_terminal_link_candidates("build.sh");
        let resolved =
            resolve_terminal_link_candidates(&candidates, Some("/remote/project"), Some("dev.ts"));

        let Some(ResolvedTerminalLink::NeedsDisambiguation {
            candidates,
            existing,
        }) = resolved
        else {
            panic!("remote build.sh should require disambiguation");
        };

        assert_eq!(existing, None);
        assert_eq!(
            terminal_link_disambiguation_items(&candidates, existing.as_deref()),
            vec![TerminalLinkDisambiguationItem::Url {
                label: "Open https://build.sh".to_string(),
                url: "https://build.sh".to_string(),
            }]
        );
    }

    #[test]
    fn test_terminal_url_regex_flags_include_vte_required_multiline() {
        assert_eq!(
            terminal_url_regex_compile_flags() & PCRE2_MULTILINE,
            PCRE2_MULTILINE
        );
    }

    #[test]
    fn terminal_link_regexes_compile_once_per_thread() {
        // Force init twice on this thread; the compile count must not grow the
        // second time, proving the PCRE2 patterns are shared, not recompiled
        // per terminal. Tolerant of a headless compile Err (stores None, still
        // counts one init).
        with_terminal_link_regexes(|_| {});
        let first = TERMINAL_LINK_REGEX_COMPILES.with(|c| c.get());
        with_terminal_link_regexes(|_| {});
        let second = TERMINAL_LINK_REGEX_COMPILES.with(|c| c.get());
        assert!(first >= 1);
        assert_eq!(
            first, second,
            "link regexes must not recompile after first init"
        );
    }

    #[test]
    fn test_hyperlink_terminal_links_allow_safe_non_http_schemes() {
        assert_eq!(
            launchable_uri_for_terminal_link(
                "mailto:test@example.com",
                TerminalLinkKind::Hyperlink
            ),
            Some("mailto:test@example.com".to_string())
        );
        assert_eq!(
            launchable_uri_for_terminal_link("file:///tmp/report.txt", TerminalLinkKind::Hyperlink),
            Some("file:///tmp/report.txt".to_string())
        );
    }

    #[test]
    fn test_hyperlink_terminal_links_reject_dangerous_schemes() {
        assert_eq!(
            launchable_uri_for_terminal_link("javascript:alert(1)", TerminalLinkKind::Hyperlink),
            None
        );
        assert_eq!(
            launchable_uri_for_terminal_link("data:text/html,hi", TerminalLinkKind::Hyperlink),
            None
        );
    }

    #[test]
    fn osc8_hyperlinks_require_full_destination_confirmation() {
        assert_eq!(
            terminal_hyperlink_confirmation_item("https://evil.example/landing?q=1"),
            Some(TerminalLinkDisambiguationItem::Hyperlink {
                label: "Open https://evil.example/landing?q=1".to_string(),
                uri: "https://evil.example/landing?q=1".to_string(),
            })
        );
        assert_eq!(
            terminal_hyperlink_confirmation_item("file:///tmp/report_with_notes.txt"),
            Some(TerminalLinkDisambiguationItem::Hyperlink {
                label: "Open file:///tmp/report__with__notes.txt".to_string(),
                uri: "file:///tmp/report_with_notes.txt".to_string(),
            }),
            "file hyperlinks must disclose their full target before editor activation",
        );
        assert_eq!(
            terminal_hyperlink_confirmation_item("mailto:test@example.com"),
            Some(TerminalLinkDisambiguationItem::Hyperlink {
                label: "Open mailto:test@example.com".to_string(),
                uri: "mailto:test@example.com".to_string(),
            }),
            "accepted non-HTTP hyperlinks must use the same confirmation path",
        );
        assert_eq!(
            terminal_hyperlink_confirmation_item("javascript:alert(1)"),
            None,
            "unsupported targets must remain inert rather than becoming confirmable",
        );
    }

    #[test]
    fn terminal_link_confirmation_activation_is_one_shot() {
        let guard = TerminalLinkActivationGuard::default();
        assert!(
            guard.claim(),
            "the explicit confirmation should activate once"
        );
        assert!(
            !guard.claim(),
            "duplicate action delivery must not open the destination again"
        );
    }

    #[test]
    fn test_local_selection_drag_is_selection_first_on_plain_drag() {
        assert!(should_capture_plain_selection_drag(
            gdk::ModifierType::empty()
        ));
        assert!(!should_capture_plain_selection_drag(
            gdk::ModifierType::SHIFT_MASK
        ));
        assert!(!should_capture_plain_selection_drag(
            gdk::ModifierType::CONTROL_MASK
        ));
        assert!(!should_capture_plain_selection_drag(
            gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK
        ));
    }

    #[test]
    fn test_local_selection_drag_threshold_ignores_clicks() {
        assert!(!local_selection_drag_moved_enough(10.0, 10.0, 11.0, 11.0));
        assert!(local_selection_drag_moved_enough(10.0, 10.0, 13.0, 10.0));
    }

    #[test]
    fn test_terminal_grid_point_clamps_to_visible_grid() {
        assert_eq!(
            terminal_grid_point_at(40, 24, 80, 10, 20, 25.0, 45.0),
            Some(TerminalGridPoint { row: 42, col: 2 })
        );
        assert_eq!(
            terminal_grid_point_at(40, 24, 80, 10, 20, -5.0, -5.0),
            Some(TerminalGridPoint { row: 40, col: 0 })
        );
        assert_eq!(
            terminal_grid_point_at(40, 24, 80, 10, 20, 9999.0, 9999.0),
            Some(TerminalGridPoint { row: 63, col: 79 })
        );
        assert_eq!(terminal_grid_point_at(0, 0, 80, 10, 20, 1.0, 1.0), None);
    }

    #[test]
    fn test_terminal_text_range_between_points_normalizes_reverse_drags() {
        assert_eq!(
            terminal_text_range_between_points(
                TerminalGridPoint { row: 3, col: 10 },
                TerminalGridPoint { row: 2, col: 5 }
            ),
            Some(TerminalTextRange {
                start_row: 2,
                start_col: 5,
                end_row: 3,
                end_col: 10,
            })
        );
        assert_eq!(
            terminal_text_range_between_points(
                TerminalGridPoint { row: 3, col: 10 },
                TerminalGridPoint { row: 3, col: 10 }
            ),
            None
        );
    }

    #[test]
    fn test_local_selection_rects_span_visible_rows() {
        let range = TerminalTextRange {
            start_row: 41,
            start_col: 2,
            end_row: 43,
            end_col: 4,
        };

        assert_eq!(
            local_selection_rects(
                range,
                LocalSelectionGeometry {
                    visible_start_row: 40,
                    rows: 5,
                    cols: 10,
                    char_width: 8,
                    char_height: 16,
                    x_offset: 3.0,
                    y_offset: 5.0,
                },
            ),
            vec![
                LocalSelectionRect {
                    x: 19.0,
                    y: 21.0,
                    width: 64.0,
                    height: 16.0,
                },
                LocalSelectionRect {
                    x: 3.0,
                    y: 37.0,
                    width: 80.0,
                    height: 16.0,
                },
                LocalSelectionRect {
                    x: 3.0,
                    y: 53.0,
                    width: 40.0,
                    height: 16.0,
                },
            ]
        );
    }

    #[test]
    fn test_resolved_target_pane_id_falls_back_when_focus_is_stale() {
        let pane = PaneNode::Stub { pane_id: 17 };
        assert_eq!(resolved_target_pane_id(&pane, 99), Some(17));
    }

    #[test]
    fn test_ssh_command_host_reads_target_after_options() {
        let command = vec![
            "ssh".to_string(),
            "-tt".to_string(),
            "-p".to_string(),
            "2222".to_string(),
            "user@devbox".to_string(),
            "cd /srv/api".to_string(),
        ];
        assert_eq!(ssh_command_host(&command), Some("devbox".to_string()));
    }

    #[test]
    fn test_ssh_command_host_skips_clustered_option_values() {
        for command in [
            vec!["ssh", "-vp2222", "user@devbox", "exec bash"],
            vec!["ssh", "-vp", "2222", "user@devbox", "exec bash"],
        ] {
            let command = command.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(ssh_command_host(&command), Some("devbox".to_string()));
        }
    }

    #[test]
    fn test_restored_ssh_command_replays_saved_remote_directory() {
        let command = vec!["ssh".to_string(), "user@devbox".to_string()];
        let restored = restored_ssh_command(&command, Some("file://devbox/srv/api"));
        assert_eq!(restored[0], "ssh");
        assert_eq!(restored[1], "-tt");
        assert_eq!(restored[2], "user@devbox");
        assert!(restored[3].contains("cd '/srv/api'"));
        assert!(restored[3].contains("exec \"$SHELL\""));
    }

    #[test]
    fn test_restored_ssh_command_keeps_existing_remote_exec() {
        let command = vec![
            "ssh".to_string(),
            "user@devbox".to_string(),
            "htop".to_string(),
        ];
        assert_eq!(
            restored_ssh_command(&command, Some("file://devbox/srv/api")),
            command
        );
    }

    #[test]
    fn test_restored_legend_uses_visual_order_instead_of_dfs_order() {
        let saved = SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(SavedPaneNode::Split {
                direction: "horizontal".into(),
                ratio: 0.5,
                first: Box::new(leaf(Some("/tmp/top-left"), None)),
                second: Box::new(leaf(Some("/tmp/bottom-left"), None)),
            }),
            second: Box::new(leaf(Some("/tmp/right"), None)),
        };

        let legend = build_restored_tab_legend(&saved, 42).expect("legend should be built");
        let labels: Vec<String> = legend.items.into_iter().map(|item| item.label).collect();

        assert_eq!(
            labels,
            vec![
                "/tmp/top-left".to_string(),
                "/tmp/right".to_string(),
                "/tmp/bottom-left".to_string(),
            ]
        );
    }

    #[test]
    fn test_restored_legend_formats_ssh_entries() {
        let saved = leaf(
            Some("file://remote.example/var/www/app"),
            Some(vec!["ssh", "deployer@staging"]),
        );

        let legend = build_restored_tab_legend(&saved, 7).expect("ssh pane should show legend");

        assert_eq!(legend.items.len(), 1);
        assert_eq!(legend.items[0].number, 1);
        assert_eq!(legend.items[0].label, "staging:/var/www/app");
        assert!(legend.items[0].is_ssh);
    }

    #[test]
    fn test_restored_legend_skips_single_local_pane() {
        let saved = leaf(Some("/tmp/project"), None);
        assert_eq!(build_restored_tab_legend(&saved, 9), None);
    }

    #[test]
    fn test_restored_legend_skips_when_saved_metadata_is_not_useful() {
        let saved = SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(leaf(None, None)),
            second: Box::new(leaf(None, None)),
        };

        assert_eq!(build_restored_tab_legend(&saved, 11), None);
    }

    #[test]
    fn test_extract_path_from_title_ignores_taarof_branch_suffix() {
        assert_eq!(
            extract_path_from_title("user@host:~/project [taarof-git:feature/test]"),
            Some("~/project".to_string())
        );
    }

    #[test]
    fn test_extract_host_from_title_reads_remote_host() {
        assert_eq!(
            extract_host_from_title("user@devbox:~/project [taarof-git:feature/test]"),
            Some("devbox".to_string())
        );
    }

    #[test]
    fn test_location_metadata_prefers_osc7_uri() {
        assert_eq!(
            location_metadata_from_terminal_state(
                Some("file://remote.example/tmp/user/project"),
                Some("user@other:~/ignored"),
            ),
            (
                Some("/tmp/user/project".to_string()),
                Some("remote.example".to_string()),
            )
        );
    }

    #[test]
    fn test_location_metadata_falls_back_to_title_when_uri_missing() {
        assert_eq!(
            location_metadata_from_terminal_state(
                None,
                Some("user@devbox:~/project [taarof-git:feature/test]"),
            ),
            (Some("~/project".to_string()), Some("devbox".to_string()))
        );
    }

    #[test]
    fn test_extract_taarof_branch_from_title_reads_branch_suffix() {
        assert_eq!(
            extract_taarof_branch_from_title("user@host:~/project [taarof-git:feature/test]"),
            Some("feature/test".to_string())
        );
    }

    #[test]
    fn test_extract_taarof_branch_from_title_requires_suffix() {
        assert_eq!(
            extract_taarof_branch_from_title("user@host:~/project"),
            None
        );
    }

    #[test]
    fn test_parse_termprop_activity_state_accepts_expected_values() {
        assert_eq!(
            parse_termprop_activity_state(Some("running")),
            Some(AgentActivityState::Running)
        );
        assert_eq!(
            parse_termprop_activity_state(Some("waiting-input")),
            Some(AgentActivityState::WaitingInput)
        );
        assert_eq!(
            parse_termprop_activity_state(Some("error")),
            Some(AgentActivityState::Errored)
        );
        assert_eq!(
            parse_termprop_activity_state(Some("DONE")),
            Some(AgentActivityState::Done)
        );
        assert_eq!(
            parse_termprop_activity_state(Some("idle")),
            Some(AgentActivityState::Idle)
        );
    }

    #[test]
    fn test_parse_termprop_activity_state_rejects_unknown_values() {
        assert_eq!(parse_termprop_activity_state(Some("thinking")), None);
        assert_eq!(parse_termprop_activity_state(None), None);
    }

    #[test]
    fn test_is_taarof_activity_termprop_matches_phase_three_props() {
        assert!(is_taarof_activity_termprop("vte.ext.taarof.agent.state"));
        assert!(is_taarof_activity_termprop("vte.ext.taarof.agent.text"));
        assert!(is_taarof_activity_termprop("vte.ext.taarof.agent.source"));
        assert!(!is_taarof_activity_termprop("vte.shell.precmd"));
    }

    #[test]
    fn dispatch_targets_default_to_focused_pane() {
        let targets = resolve_dispatch_target_ids(
            7,
            Some(11),
            None,
            &[DispatchTabSnapshot {
                workspace_id: 7,
                tab_id: 11,
                focused_pane_id: 102,
                pane_ids: vec![101, 102, 103],
            }],
        );
        assert_eq!(
            targets,
            vec![DispatchTargetId {
                tab_id: 11,
                pane_id: 102,
            }]
        );
    }

    #[test]
    fn dispatch_targets_include_all_panes_in_active_tab_scope() {
        let targets = resolve_dispatch_target_ids(
            7,
            Some(11),
            Some(BroadcastScope::Tab),
            &[
                DispatchTabSnapshot {
                    workspace_id: 7,
                    tab_id: 11,
                    focused_pane_id: 102,
                    pane_ids: vec![101, 102, 103],
                },
                DispatchTabSnapshot {
                    workspace_id: 7,
                    tab_id: 12,
                    focused_pane_id: 201,
                    pane_ids: vec![201],
                },
            ],
        );
        assert_eq!(
            targets,
            vec![
                DispatchTargetId {
                    tab_id: 11,
                    pane_id: 101,
                },
                DispatchTargetId {
                    tab_id: 11,
                    pane_id: 102,
                },
                DispatchTargetId {
                    tab_id: 11,
                    pane_id: 103,
                },
            ]
        );
    }

    #[test]
    fn dispatch_targets_include_all_panes_in_active_workspace_scope() {
        let targets = resolve_dispatch_target_ids(
            7,
            Some(11),
            Some(BroadcastScope::Workspace),
            &[
                DispatchTabSnapshot {
                    workspace_id: 7,
                    tab_id: 11,
                    focused_pane_id: 102,
                    pane_ids: vec![101, 102],
                },
                DispatchTabSnapshot {
                    workspace_id: 7,
                    tab_id: 12,
                    focused_pane_id: 201,
                    pane_ids: vec![201],
                },
                DispatchTabSnapshot {
                    workspace_id: 8,
                    tab_id: 21,
                    focused_pane_id: 301,
                    pane_ids: vec![301, 302],
                },
            ],
        );
        assert_eq!(
            targets,
            vec![
                DispatchTargetId {
                    tab_id: 11,
                    pane_id: 101,
                },
                DispatchTargetId {
                    tab_id: 11,
                    pane_id: 102,
                },
                DispatchTargetId {
                    tab_id: 12,
                    pane_id: 201,
                },
            ]
        );
    }

    #[test]
    fn dispatch_targets_return_to_single_pane_after_broadcast_turns_off() {
        let tabs = vec![DispatchTabSnapshot {
            workspace_id: 7,
            tab_id: 11,
            focused_pane_id: 103,
            pane_ids: vec![101, 102, 103],
        }];
        let broadcast_targets =
            resolve_dispatch_target_ids(7, Some(11), Some(BroadcastScope::Tab), &tabs);
        assert_eq!(broadcast_targets.len(), 3);

        let focused_targets = resolve_dispatch_target_ids(7, Some(11), None, &tabs);
        assert_eq!(
            focused_targets,
            vec![DispatchTargetId {
                tab_id: 11,
                pane_id: 103,
            }]
        );
    }

    #[test]
    fn commit_forwarding_requires_non_empty_text_and_broadcast_scope() {
        assert!(!should_broadcast_commit("", Some(BroadcastScope::Tab)));
        assert!(!should_broadcast_commit("scroll-report", None));
        assert!(should_broadcast_commit(
            "scroll-report",
            Some(BroadcastScope::Tab)
        ));
        assert!(should_broadcast_commit(
            "scroll-report",
            Some(BroadcastScope::Workspace)
        ));
    }

    #[test]
    #[ignore] // requires tmux to be installed
    fn test_tmux_session_exists_returns_false_for_nonexistent() {
        let exists = tmux_session_exists(
            &crate::tmux::TmuxTarget::Local,
            "taarof-nonexistent-session-12345",
        );
        assert!(!exists);
    }

    #[test]
    fn test_pane_spawn_argv_with_remote_host() {
        let host = crate::host::HostConfig {
            name: "devbox".to_string(),
            ssh_target: Some("user@devbox.example.com".to_string()),
            ..Default::default()
        };
        let cfg = GhosttyConfig::default();
        let (argv, backing) =
            pane_spawn_argv(&cfg, true, Some("test-session"), Some("/tmp"), Some(&host));
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-t");
        assert_eq!(argv[2], "user@devbox.example.com");
        assert_eq!(argv[3], "tmux");
        let backing = backing.unwrap();
        assert_eq!(backing.session_name, "test-session");
        match backing.target {
            crate::tmux::TmuxTarget::Remote { ssh_target } => {
                assert_eq!(ssh_target, "user@devbox.example.com");
            }
            _ => panic!("expected remote target"),
        }
    }

    #[test]
    fn test_pane_spawn_argv_local_no_host() {
        let cfg = GhosttyConfig::default();
        let (argv, backing) = pane_spawn_argv(&cfg, true, Some("test-session"), Some("/tmp"), None);
        assert_eq!(argv[0], "tmux");
        let backing = backing.unwrap();
        assert_eq!(backing.target, crate::tmux::TmuxTarget::Local);
    }

    #[test]
    fn test_resolve_attach_target_prefers_active_terminal_tab() {
        let tabs = vec![(10, TabKind::Terminal), (20, TabKind::Dashboard)];
        assert_eq!(
            resolve_attach_target_tab_id_in_workspace(10, Some(20), &tabs),
            Some(10)
        );
    }

    #[test]
    fn test_resolve_attach_target_falls_back_to_last_active_terminal_tab() {
        let tabs = vec![(10, TabKind::Terminal), (20, TabKind::Dashboard)];
        assert_eq!(
            resolve_attach_target_tab_id_in_workspace(20, Some(10), &tabs),
            Some(10)
        );
    }

    #[test]
    fn test_resolve_attach_target_falls_back_to_first_terminal_tab() {
        let tabs = vec![
            (10, TabKind::Dashboard),
            (20, TabKind::Terminal),
            (30, TabKind::Terminal),
        ];
        assert_eq!(
            resolve_attach_target_tab_id_in_workspace(10, Some(999), &tabs),
            Some(20)
        );
    }

    #[test]
    fn test_resolve_attach_target_returns_none_without_terminal_tabs() {
        let tabs = vec![(10, TabKind::Dashboard), (20, TabKind::Dashboard)];
        assert_eq!(
            resolve_attach_target_tab_id_in_workspace(10, Some(20), &tabs),
            None
        );
    }

    #[test]
    fn test_attach_session_error_display_no_terminal_tab() {
        assert_eq!(
            super::AttachSessionError::NoTerminalTabAvailable.to_string(),
            "no terminal tab available in active workspace"
        );
    }

    #[test]
    fn test_attach_session_error_display_session_unavailable() {
        assert_eq!(
            super::AttachSessionError::SessionUnavailable {
                session_name: "taarof--build--t7--0".to_string(),
                target: "user@ci.internal".to_string(),
            }
            .to_string(),
            "tmux session taarof--build--t7--0 is not available on user@ci.internal"
        );
    }

    #[test]
    fn test_resolve_attached_session_pane_in_list_returns_matching_tab_and_pane() {
        let panes = vec![
            AttachedSessionPane {
                tab_id: 11,
                pane_id: 3,
                session_name: "taarof--alpha--t11--3".to_string(),
                target: crate::tmux::TmuxTarget::Local,
            },
            AttachedSessionPane {
                tab_id: 12,
                pane_id: 4,
                session_name: "taarof--beta--t12--4".to_string(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".to_string(),
                },
            },
        ];
        assert_eq!(
            resolve_attached_session_pane_in_list(
                "taarof--beta--t12--4",
                &crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".to_string(),
                },
                &panes,
            ),
            Some((12, 4))
        );
    }

    #[test]
    fn test_resolve_attached_session_pane_in_list_returns_none_when_missing() {
        let panes = vec![AttachedSessionPane {
            tab_id: 11,
            pane_id: 3,
            session_name: "taarof--alpha--t11--3".to_string(),
            target: crate::tmux::TmuxTarget::Local,
        }];
        assert_eq!(
            resolve_attached_session_pane_in_list(
                "taarof--missing--t99--0",
                &crate::tmux::TmuxTarget::Local,
                &panes,
            ),
            None
        );
    }

    #[test]
    fn test_resolve_attached_session_pane_in_list_disambiguates_same_name_by_target() {
        let panes = vec![
            AttachedSessionPane {
                tab_id: 11,
                pane_id: 3,
                session_name: "taarof--shared--t11--3".to_string(),
                target: crate::tmux::TmuxTarget::Local,
            },
            AttachedSessionPane {
                tab_id: 12,
                pane_id: 4,
                session_name: "taarof--shared--t11--3".to_string(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".to_string(),
                },
            },
        ];

        assert_eq!(
            resolve_attached_session_pane_in_list(
                "taarof--shared--t11--3",
                &crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".to_string(),
                },
                &panes,
            ),
            Some((12, 4))
        );
    }

    #[test]
    fn test_resolve_session_target_prefers_live_remote_backing() {
        let target = resolve_session_target(
            &[],
            &[(
                "taarof--remote--t5--0".to_string(),
                crate::tmux::TmuxTarget::Remote {
                    ssh_target: "user@remote.example".to_string(),
                },
            )],
            "taarof--remote--t5--0",
            None,
        );
        assert_eq!(
            target,
            crate::tmux::TmuxTarget::Remote {
                ssh_target: "user@remote.example".to_string()
            }
        );
    }

    #[test]
    fn test_resolve_session_target_prefers_explicit_hint() {
        let target = resolve_session_target(
            &[],
            &[("ignored".to_string(), crate::tmux::TmuxTarget::Local)],
            "ignored",
            Some(&crate::tmux::TmuxTarget::Remote {
                ssh_target: "user@hint.example".to_string(),
            }),
        );
        assert_eq!(
            target,
            crate::tmux::TmuxTarget::Remote {
                ssh_target: "user@hint.example".to_string()
            }
        );
    }

    #[test]
    fn test_run_tmux_command_with_echo() {
        // Use 'echo' as a proxy for tmux commands
        let argv = vec!["echo".to_string(), "hello world".to_string()];
        let result = run_tmux_command(&argv);
        assert_eq!(result.as_deref(), Some("hello world\n"));
    }

    #[test]
    fn test_run_tmux_command_empty_argv() {
        let result = run_tmux_command(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_run_tmux_command_bad_command() {
        let argv = vec!["nonexistent-command-12345".to_string()];
        let result = run_tmux_command(&argv);
        assert!(result.is_none());
    }

    #[test]
    fn tmux_kill_result_classifies_success_missing_and_transient_failures() {
        assert_eq!(
            classify_tmux_kill_result(true, "", || "unused".to_string()),
            TmuxKillResult::Killed
        );
        assert_eq!(
            classify_tmux_kill_result(false, "no server running on /tmp/tmux-1000/default", || {
                "unused".to_string()
            }),
            TmuxKillResult::AlreadyMissing
        );
        assert_eq!(
            classify_tmux_kill_result(false, "can't find session: gone", || {
                "unused".to_string()
            }),
            TmuxKillResult::AlreadyMissing
        );
        assert_eq!(
            classify_tmux_kill_result(
                false,
                "error connecting to /tmp/tmux-1000/default (No such file or directory)",
                || "unused".to_string(),
            ),
            TmuxKillResult::AlreadyMissing
        );
        assert_eq!(
            classify_tmux_kill_result(false, "No such file or directory", || {
                "local exec failure".to_string()
            }),
            TmuxKillResult::TransientFailure("local exec failure".to_string())
        );
        assert_eq!(
            classify_tmux_kill_result(false, "ssh: connect to host timed out", || {
                "ssh timeout".to_string()
            }),
            TmuxKillResult::TransientFailure("ssh timeout".to_string())
        );
    }

    #[test]
    fn transient_kill_of_attached_session_does_not_create_detached_entry() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "attached-session",
            crate::HeadlessPaneSeed {
                tmux_session: Some("still-attached".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless attached tmux pane");
        let backing = crate::pane::TmuxBacking {
            session_name: "still-attached".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };

        let error = kill_session_by_name_with_hint_using(
            &state,
            &backing.session_name,
            Some(&backing.target),
            |target, session_name| {
                assert_eq!(target, &backing.target);
                assert_eq!(session_name, backing.session_name);
                TmuxKillResult::TransientFailure("tmux timed out".to_string())
            },
        )
        .expect_err("transient failure must be surfaced");

        assert!(state.borrow().detached_sessions.is_empty());
        assert!(error.contains("tmux timed out"));
        assert!(!error.contains("preserved it in Background"));
    }

    #[test]
    fn transient_kill_on_pane_teardown_still_preserves_detached() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let backing = crate::pane::TmuxBacking {
            session_name: "removed-inactive-pane".to_string(),
            target: TmuxTarget::Remote {
                ssh_target: "builder@example".to_string(),
            },
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };

        let error = apply_tmux_kill_result_in_workspace(
            &state,
            &backing,
            TmuxKillResult::TransientFailure("ssh timeout".to_string()),
            Some("inactive-source"),
        )
        .expect_err("transient teardown failure must be surfaced");

        assert!(error.contains("preserved it in Background"));
        let st = state.borrow();
        assert_eq!(st.detached_sessions.len(), 1);
        let preserved = &st.detached_sessions[0];
        assert!(preserved.matches_target(&backing.session_name, &backing.target));
        assert_eq!(preserved.workspace, "inactive-source");
        assert_eq!(preserved.host, "builder@example");
    }

    #[test]
    fn transient_kill_removes_existing_detached_entry_for_live_session() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "attached-session",
            crate::HeadlessPaneSeed {
                tmux_session: Some("live-with-stale-entry".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless attached tmux pane");
        let backing = crate::pane::TmuxBacking {
            session_name: "live-with-stale-entry".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let original_detached_at = Instant::now();
        state.borrow_mut().detached_sessions.push(DetachedSession {
            session_name: backing.session_name.clone(),
            host: "localhost".to_string(),
            workspace: "original-workspace".to_string(),
            target: backing.target.clone(),
            detached_at: original_detached_at,
            last_command: Some("codex".to_string()),
            finished: true,
        });

        let error = apply_tmux_kill_result_in_workspace(
            &state,
            &backing,
            TmuxKillResult::TransientFailure("tmux timed out".to_string()),
            Some("replacement-workspace"),
        )
        .expect_err("transient failure must be surfaced");

        assert!(!error.contains("preserved it in Background"));
        let st = state.borrow();
        assert!(st.detached_sessions.is_empty());
        assert!(crate::sidebar::background_session_names(&st).is_empty());
    }

    #[test]
    fn transient_kill_of_matching_headless_remote_pane_skips_preserve() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "remote-attached-session",
            crate::HeadlessPaneSeed {
                tmux_session: Some("remote-still-attached".to_string()),
                tmux_ssh_target: Some("builder@example".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless remote tmux pane");
        let backing = crate::pane::TmuxBacking {
            session_name: "remote-still-attached".to_string(),
            target: TmuxTarget::Remote {
                ssh_target: "builder@example".to_string(),
            },
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };

        let error = apply_tmux_kill_result(
            &state,
            &backing,
            TmuxKillResult::TransientFailure("ssh timeout".to_string()),
        )
        .expect_err("transient failure must be surfaced");

        assert!(state.borrow().detached_sessions.is_empty());
        assert!(!error.contains("preserved it in Background"));
    }

    #[test]
    fn transient_kill_failures_preserve_local_and_remote_sessions_as_discoverable() {
        for (target, error) in [
            (TmuxTarget::Local, "local exec failure"),
            (
                TmuxTarget::Remote {
                    ssh_target: "builder".to_string(),
                },
                "ssh timeout",
            ),
        ] {
            let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
            let backing = crate::pane::TmuxBacking {
                session_name: "preserved-after-failure".to_string(),
                target: target.clone(),
                expected_generation: None,
                pane_info: crate::probe::ProbeSnapshot::default(),
            };
            state.borrow_mut().dashboard_state = crate::dashboard::aggregate_dashboard_state(
                &[],
                &[],
                &[(
                    target.clone(),
                    vec![(backing.session_name.clone(), 1, false, 1)],
                )],
                &std::collections::HashMap::new(),
                "taarof",
            );
            let poll_context = state
                .borrow_mut()
                .begin_dashboard_poll(std::slice::from_ref(&target));

            let cleanup_error = apply_tmux_kill_result(
                &state,
                &backing,
                TmuxKillResult::TransientFailure(error.to_string()),
            )
            .expect_err("transient failure must be surfaced");

            let st = state.borrow();
            assert!(cleanup_error.contains(error));
            assert!(st
                .dashboard_poll_tracker
                .result_is_current(&poll_context, &target));
            assert!(st
                .detached_sessions
                .iter()
                .any(|session| session.matches_target(&backing.session_name, &target)));
            assert!(st.dashboard_state.sessions.iter().any(|session| {
                session.name == backing.session_name && session.target == target
            }));
        }
    }

    #[test]
    fn transient_kill_of_finished_detached_session_preserves_finished_flag() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let backing = crate::pane::TmuxBacking {
            session_name: "finished-after-failure".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let original_detached_at = Instant::now() - Duration::from_secs(60);
        state.borrow_mut().detached_sessions.push(DetachedSession {
            session_name: backing.session_name.clone(),
            host: "localhost".to_string(),
            workspace: "default".to_string(),
            target: backing.target.clone(),
            detached_at: original_detached_at,
            last_command: Some("cargo build".to_string()),
            finished: true,
        });

        apply_tmux_kill_result(
            &state,
            &backing,
            TmuxKillResult::TransientFailure("local exec failure".to_string()),
        )
        .expect_err("transient failure must be surfaced");

        let st = state.borrow();
        assert_eq!(st.detached_sessions.len(), 1);
        let preserved = &st.detached_sessions[0];
        assert!(preserved.finished);
        assert_eq!(preserved.last_command.as_deref(), Some("cargo build"));
        assert!(preserved.detached_at > original_detached_at);
    }

    #[test]
    fn failed_kill_then_poll_does_not_renotify_finished_session() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let finished_backing = crate::pane::TmuxBacking {
            session_name: "finished-after-failure".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let unfinished_backing = crate::pane::TmuxBacking {
            session_name: "first-finish".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let original_detached_at = Instant::now() - Duration::from_secs(60);
        state.borrow_mut().detached_sessions.extend([
            DetachedSession {
                session_name: finished_backing.session_name.clone(),
                host: "localhost".to_string(),
                workspace: "default".to_string(),
                target: finished_backing.target.clone(),
                detached_at: original_detached_at,
                last_command: Some("cargo build".to_string()),
                finished: true,
            },
            DetachedSession {
                session_name: unfinished_backing.session_name.clone(),
                host: "localhost".to_string(),
                workspace: "default".to_string(),
                target: unfinished_backing.target.clone(),
                detached_at: original_detached_at,
                last_command: Some("cargo test".to_string()),
                finished: false,
            },
        ]);

        apply_tmux_kill_result(
            &state,
            &finished_backing,
            TmuxKillResult::TransientFailure("local exec failure".to_string()),
        )
        .expect_err("transient failure must be surfaced");
        apply_tmux_kill_result(
            &state,
            &unfinished_backing,
            TmuxKillResult::TransientFailure("local exec failure".to_string()),
        )
        .expect_err("transient failure must be surfaced");

        let mut commands = std::collections::HashMap::new();
        commands.insert(
            crate::dashboard::detached_session_command_key(
                &finished_backing.target,
                &finished_backing.session_name,
            ),
            "bash".to_string(),
        );
        commands.insert(
            crate::dashboard::detached_session_command_key(
                &unfinished_backing.target,
                &unfinished_backing.session_name,
            ),
            "bash".to_string(),
        );
        let mut st = state.borrow_mut();
        let newly_finished =
            crate::dashboard::check_and_notify_finished(&mut st.detached_sessions, &commands);

        assert_eq!(newly_finished, vec!["first-finish"]);
        assert!(st.detached_sessions.iter().all(|session| session.finished));
    }

    #[test]
    fn transient_close_pane_preserves_inactive_source_workspace_in_background() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let active_workspace = state.borrow().active_workspace;
        let source_workspace = state.borrow_mut().create_workspace("inactive-source", None);
        state.borrow_mut().active_workspace = active_workspace;
        assert_ne!(source_workspace, state.borrow().active_workspace);

        let backing = crate::pane::TmuxBacking {
            session_name: "preserved-inactive-pane".to_string(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let error = apply_tmux_kill_result_in_workspace(
            &state,
            &backing,
            TmuxKillResult::TransientFailure("local exec failure".to_string()),
            Some("inactive-source"),
        )
        .expect_err("transient pane-close kill must be surfaced");

        assert!(error.contains("preserved it in Background"));
        let st = state.borrow();
        let preserved = st
            .detached_sessions
            .iter()
            .find(|session| session.matches_target(&backing.session_name, &backing.target))
            .expect("failed close must remain discoverable");
        assert_eq!(preserved.workspace, "inactive-source");
        assert_eq!(
            crate::sidebar::background_session_names(&st),
            vec![backing.session_name]
        );
    }

    #[test]
    fn authoritative_close_pane_kill_results_still_remove_session() {
        for result in [TmuxKillResult::Killed, TmuxKillResult::AlreadyMissing] {
            let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
            let backing = crate::pane::TmuxBacking {
                session_name: "closed-pane".to_string(),
                target: TmuxTarget::Local,
                expected_generation: None,
                pane_info: crate::probe::ProbeSnapshot::default(),
            };
            state.borrow_mut().detached_sessions.push(DetachedSession {
                session_name: backing.session_name.clone(),
                host: "localhost".to_string(),
                workspace: "inactive-source".to_string(),
                target: backing.target.clone(),
                detached_at: std::time::Instant::now(),
                last_command: None,
                finished: false,
            });

            apply_tmux_kill_result_in_workspace(&state, &backing, result, Some("inactive-source"))
                .expect("authoritative pane-close result should close normally");

            assert!(state.borrow().detached_sessions.is_empty());
        }
    }

    #[test]
    fn pane_close_cleanup_rejects_inflight_poll_snapshot() {
        // A confirmed-missing session is as authoritative as a successful kill:
        // discard its tracker row and reject results from the old lifecycle.
        let backing = crate::pane::TmuxBacking {
            session_name: "test-nonexistent-session".to_string(),
            target: crate::tmux::TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        state.borrow_mut().detached_sessions.push(DetachedSession {
            session_name: backing.session_name.clone(),
            host: "localhost".to_string(),
            workspace: "test".to_string(),
            target: backing.target.clone(),
            detached_at: std::time::Instant::now(),
            last_command: None,
            finished: false,
        });
        state.borrow_mut().dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &[],
            &[(
                TmuxTarget::Local,
                vec![(backing.session_name.clone(), 1, false, 1)],
            )],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[TmuxTarget::Local]);

        apply_tmux_kill_result(&state, &backing, TmuxKillResult::AlreadyMissing)
            .expect("confirmed missing is an authoritative close");
        {
            let mut st = state.borrow_mut();
            apply_dashboard_poll_results(
                &mut st,
                DashboardPollOutcome {
                    current_commands: &std::collections::HashMap::new(),
                    polled_detached_sessions: &[],
                    live_sessions: &[(
                        TmuxTarget::Local,
                        vec![(backing.session_name.clone(), 1, false, 1)],
                    )],
                    detached_probe_errors: &[],
                    dashboard_errors: &[],
                    dashboard_target_count: 1,
                    detached_target_count: 0,
                    session_prefix: "taarof",
                    poll_context: &poll_context,
                },
            );
        }

        assert!(state.borrow().dashboard_state.sessions.is_empty());
        assert!(state.borrow().detached_sessions.is_empty());
        assert!(!state
            .borrow()
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &TmuxTarget::Local));
    }

    #[test]
    fn test_cleanup_tmux_backing_detach_does_not_panic() {
        // When close_behavior is Detach (no-op), should also not panic.
        let backing = crate::pane::TmuxBacking {
            session_name: "test-detach-session".to_string(),
            target: crate::tmux::TmuxTarget::Local,
            expected_generation: None,
            pane_info: crate::probe::ProbeSnapshot::default(),
        };
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        state.borrow_mut().dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &[],
            &[(
                TmuxTarget::Local,
                vec![(backing.session_name.clone(), 1, false, 1)],
            )],
            &std::collections::HashMap::new(),
            "taarof",
        );
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[TmuxTarget::Local]);

        super::cleanup_tmux_backing_with_behavior(
            &state,
            &backing,
            crate::config::TmuxCloseBehavior::Detach,
        )
        .expect("detach cleanup is a no-op");

        assert_eq!(state.borrow().dashboard_state.sessions.len(), 1);
        assert!(state
            .borrow()
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &TmuxTarget::Local));
        // Should not panic — detach is a no-op
    }

    #[test]
    fn tab_close_cleanup_tombstones_every_target_before_removal() {
        let state = std::rc::Rc::new(std::cell::RefCell::new(AppState::new()));
        let backings = vec![
            crate::pane::TmuxBacking {
                session_name: "tab-local".to_string(),
                target: TmuxTarget::Local,
                expected_generation: None,
                pane_info: crate::probe::ProbeSnapshot::default(),
            },
            crate::pane::TmuxBacking {
                session_name: "tab-local-second-pane".to_string(),
                target: TmuxTarget::Local,
                expected_generation: None,
                pane_info: crate::probe::ProbeSnapshot::default(),
            },
        ];
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[TmuxTarget::Local]);

        for backing in &backings {
            apply_tmux_kill_result(&state, backing, TmuxKillResult::Killed)
                .expect("successful tab close kill should tombstone");
        }

        let st = state.borrow();
        assert!(!st
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &TmuxTarget::Local));
    }

    #[test]
    fn explicit_tmux_tab_ui_state_is_available_when_local_tmux_exists() {
        let state = super::explicit_tmux_tab_ui_state_for(true, false);
        assert!(state.available);
        assert!(state.local_target_available);
        assert!(!state.has_remote_targets);
        assert_eq!(state.tooltip, "Create a tmux-backed tab");
        assert_eq!(state.unavailable_message, None);
    }

    #[test]
    fn explicit_tmux_tab_ui_state_is_available_with_local_and_remote() {
        let state = super::explicit_tmux_tab_ui_state_for(true, true);
        assert!(state.available);
        assert!(state.local_target_available);
        assert!(state.has_remote_targets);
        assert_eq!(state.tooltip, "Create a tmux-backed tab");
        assert_eq!(state.unavailable_message, None);
    }

    #[test]
    fn explicit_tmux_tab_ui_state_keeps_remote_only_entrypoints_usable() {
        let state = super::explicit_tmux_tab_ui_state_for(false, true);
        assert!(state.available);
        assert!(!state.local_target_available);
        assert!(state.has_remote_targets);
        assert!(state.tooltip.contains("configured remote host"));
        assert_eq!(state.unavailable_message, None);
    }

    #[test]
    fn explicit_tmux_tab_ui_state_disables_entry_when_no_targets_exist() {
        let state = super::explicit_tmux_tab_ui_state_for(false, false);
        assert!(!state.available);
        assert!(!state.local_target_available);
        assert!(!state.has_remote_targets);
        assert_eq!(
            state.unavailable_message.as_deref(),
            Some(
                "tmux is not available on this machine and no remote tmux hosts are configured. Install tmux or add a Host entry to ~/.ssh/config to create tmux-backed tabs."
            )
        );
        assert_eq!(state.tooltip, state.unavailable_message.unwrap());
    }

    #[test]
    fn test_poll_host_status_no_hosts_is_noop() {
        // This is primarily a compile-check — the function should not panic
        // when there are no workspaces with host_config_name set.
        // Real testing requires AppState with GTK which can't be done in unit tests.
    }

    #[test]
    fn parse_path_line_col_handles_all_shapes() {
        assert_eq!(
            parse_path_line_col("src/main.rs:42"),
            Some(FileMatch {
                path: "src/main.rs".into(),
                line: 42,
                col: None
            })
        );
        assert_eq!(
            parse_path_line_col("/tmp/user:1:9"),
            Some(FileMatch {
                path: "/tmp/user".into(),
                line: 1,
                col: Some(9)
            })
        );
        // No line number -> not a file reference.
        assert_eq!(parse_path_line_col("src/main.rs"), None);
        // Non-numeric line -> rejected.
        assert_eq!(parse_path_line_col("foo:bar"), None);
        // Empty path -> rejected.
        assert_eq!(parse_path_line_col(":42"), None);
    }

    #[test]
    fn resolve_file_path_handles_absolute_relative_and_remote() {
        let abs = FileMatch {
            path: "/etc/hosts".into(),
            line: 1,
            col: None,
        };
        assert_eq!(
            resolve_file_path(&abs, Some("/tmp/user"), None),
            Some(std::path::PathBuf::from("/etc/hosts"))
        );

        let rel = FileMatch {
            path: "src/main.rs".into(),
            line: 1,
            col: None,
        };
        assert_eq!(
            resolve_file_path(&rel, Some("/tmp/user/proj"), None),
            Some(std::path::PathBuf::from("/tmp/user/proj/src/main.rs"))
        );

        // Remote pane -> cannot open in a local editor.
        assert_eq!(
            resolve_file_path(&rel, Some("/tmp/user/proj"), Some("server")),
            None
        );

        // Relative path with no known CWD -> cannot resolve.
        assert_eq!(resolve_file_path(&rel, None, None), None);
    }

    #[test]
    fn test_bare_path_candidates_require_existing_file() {
        // A unique temp dir so the existence gate has something real to hit.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("taarof-bare-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("report.md");
        std::fs::write(&file, b"x").expect("write temp file");
        let dir_str = dir.to_str().expect("temp dir is utf-8");
        let file_str = file.to_str().expect("temp file is utf-8");

        // Bare relative path that exists -> resolved against the cwd.
        assert_eq!(
            resolve_existing_bare_path("report.md", Some(dir_str), None),
            Some(file.clone())
        );
        // Absolute path that exists -> used as-is.
        assert_eq!(
            resolve_existing_bare_path(file_str, Some(dir_str), None),
            Some(file.clone())
        );
        // Lookalike that does not exist -> no target (existence is the gate).
        assert_eq!(
            resolve_existing_bare_path("nope.md", Some(dir_str), None),
            None
        );
        // Surrounding punctuation is trimmed before resolving.
        assert_eq!(
            resolve_existing_bare_path("(report.md)", Some(dir_str), None),
            Some(file.clone())
        );
        // Remote pane -> a local editor cannot open it.
        assert_eq!(
            resolve_existing_bare_path("report.md", Some(dir_str), Some("server")),
            None
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn document_resolution_gates_require_regular_files_and_accept_symlinks() {
        let dir = unique_temp_dir("taarof-document-file-gates");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let document_dir = dir.join("document-dir");
        std::fs::create_dir(&document_dir).expect("create document-like directory");
        let file = dir.join("notes.md");
        std::fs::write(&file, b"x").expect("write regular file");
        let symlink = dir.join("notes-link.md");
        std::os::unix::fs::symlink(&file, &symlink).expect("create symlink to regular file");
        let cwd = dir.to_str().expect("temp dir is utf-8");

        assert_eq!(
            resolve_existing_bare_path("document-dir/.", Some(cwd), None),
            None,
            "keyboard and bare-path activation must reject directories",
        );
        assert_eq!(
            resolve_local_file_match_with_metadata("document-dir/.:12:3", Some(cwd), None,),
            None,
            "Ctrl-click resolution must reject a path ending in /.",
        );
        assert_eq!(
            terminal_match_hover_cursor_name("document-dir/.", Some(cwd), None),
            None,
            "directories must not advertise an activatable hover cursor",
        );

        let directory_candidate = TerminalLinkCandidate::File {
            path: "document-dir/.".to_string(),
            line: Some(12),
            col: Some(3),
        };
        assert_eq!(
            resolved_file_candidate(&directory_candidate, Some(cwd), None),
            None,
            "candidate resolution must reject directory metadata",
        );

        assert_eq!(
            resolve_existing_bare_path("notes-link.md", Some(cwd), None),
            Some(symlink.clone()),
            "a symlink to a regular file remains a valid document",
        );
        assert_eq!(
            resolve_local_file_match_with_metadata("notes-link.md:8", Some(cwd), None),
            Some((symlink.clone(), 8, None)),
        );
        let symlink_candidate = TerminalLinkCandidate::File {
            path: "notes-link.md".to_string(),
            line: Some(8),
            col: None,
        };
        assert_eq!(
            resolved_file_candidate(&symlink_candidate, Some(cwd), None),
            Some((symlink.clone(), 8, None)),
        );
        assert_eq!(
            terminal_match_hover_cursor_name("./notes-link.md", Some(cwd), None),
            Some("pointer"),
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_hover_cursor_requires_an_existing_local_match() {
        let dir = unique_temp_dir("taarof-file-hover");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("report.md"), b"x").expect("write temp file");
        std::fs::write(dir.join("notes.me"), b"x").expect("write unknown-tld temp file");
        let cwd = dir.to_str().expect("temp dir is utf-8");

        assert_eq!(
            terminal_match_hover_cursor_name("report.md", Some(cwd), None),
            Some("hand2"),
            "an existing file/domain ambiguity opens the chooser",
        );
        assert_eq!(
            terminal_match_hover_cursor_name("./report.md", Some(cwd), None),
            Some("pointer")
        );
        assert_eq!(
            terminal_match_hover_cursor_name("./report.md:7:3", Some(cwd), None),
            Some("pointer")
        );
        assert_eq!(
            terminal_match_hover_cursor_name("missing.md", Some(cwd), None),
            None
        );
        assert_eq!(
            terminal_match_hover_cursor_name("report.md", Some(cwd), Some("builder.ts")),
            Some("hand2"),
            "remote ambiguity still offers its safe URL candidate",
        );
        assert_eq!(
            terminal_match_hover_cursor_name("./report.md", Some(cwd), Some("builder.ts")),
            None
        );
        assert_eq!(
            terminal_match_hover_cursor_name("https://example.com/path", Some(cwd), None),
            Some("hand2")
        );
        assert_eq!(
            terminal_match_hover_cursor_name("notes.me", Some(cwd), None),
            Some("pointer"),
            "an existing unknown-TLD file remains clickable",
        );
        assert_eq!(
            terminal_match_hover_cursor_name("missing.me", Some(cwd), None),
            None,
            "a missing unknown-TLD token must not inherit the URL regex cursor",
        );
        assert_eq!(
            terminal_match_hover_cursor_name("script.pl", Some(cwd), None),
            None,
            "another missing unknown-TLD token stays inert",
        );
        assert_eq!(
            terminal_hyperlink_hover_cursor_name("https://example.com/path"),
            Some("hand2")
        );
        assert_eq!(
            terminal_hyperlink_hover_cursor_name("javascript:alert(1)"),
            None
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_bare_path_regex_ignores_urls_and_plain_words() {
        // Path-shaped tokens: a slash or a real filename extension.
        assert!(looks_like_path_candidate("report.md"));
        assert!(looks_like_path_candidate("src/lib/foo.ts"));
        assert!(looks_like_path_candidate("./notes.txt"));
        assert!(looks_like_path_candidate("/etc/hosts"));
        // Non-paths: URLs, plain words, whitespace, numeric-only extension.
        assert!(!looks_like_path_candidate("https://example.com/x"));
        assert!(!looks_like_path_candidate("hello"));
        assert!(!looks_like_path_candidate("the"));
        assert!(!looks_like_path_candidate("just some words"));
        assert!(!looks_like_path_candidate("1.5"));
        assert!(!looks_like_path_candidate("."));
        assert!(!looks_like_path_candidate(".."));
    }

    // Serializes the few tests that mutate the process-global `HOME` env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn expand_leading_tilde_expands_only_leading_home() {
        let home = "/tmp/user";
        assert_eq!(
            expand_leading_tilde("~/proj/report.md", home),
            "/tmp/user/proj/report.md"
        );
        assert_eq!(expand_leading_tilde("~", home), "/tmp/user");
        // A trailing slash on home is not doubled.
        assert_eq!(expand_leading_tilde("~/x", "/tmp/user/"), "/tmp/user/x");
        // Only a leading `~/` (or bare `~`) expands; everything else is untouched.
        assert_eq!(expand_leading_tilde("~user/x", home), "~user/x");
        assert_eq!(
            expand_leading_tilde("/abs/report.md", home),
            "/abs/report.md"
        );
        assert_eq!(expand_leading_tilde("report.md", home), "report.md");
        assert_eq!(expand_leading_tilde("a/~/b", home), "a/~/b");
        // Empty home -> no expansion, so a `~` path never resolves into "/proj".
        assert_eq!(expand_leading_tilde("~/proj", ""), "~/proj");
    }

    #[test]
    fn resolve_tilde_cwd_from_title_fallback_expands_home() {
        // Regression: a pane with no OSC 7 falls back to the terminal *title*,
        // whose path is tilde-form (e.g. "user@host:~/proj"). Bare-path peek and
        // click-to-open must expand `~` before the existence check, or every such
        // open silently fails with "No file path under cursor to peek".
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home =
            std::env::temp_dir().join(format!("taarof-home-{}-{}", std::process::id(), nanos));
        let proj = home.join("proj");
        std::fs::create_dir_all(&proj).expect("create proj dir");
        let file = proj.join("report.md");
        std::fs::write(&file, b"x").expect("write file");

        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        // Bare path against a tilde-form cwd resolves via $HOME.
        assert_eq!(
            resolve_existing_bare_path("report.md", Some("~/proj"), None),
            Some(file.clone())
        );
        // A tilde-form path token itself also expands (cwd irrelevant).
        assert_eq!(
            resolve_existing_bare_path("~/proj/report.md", None, None),
            Some(file.clone())
        );
        // The `path:line` resolver expands the tilde cwd too.
        let m = FileMatch {
            path: "report.md".into(),
            line: 5,
            col: None,
        };
        assert_eq!(
            resolve_file_path(&m, Some("~/proj"), None),
            Some(file.clone())
        );

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn token_at_column_extracts_word_under_index() {
        let row = "wrote src/lib/foo.ts done";
        // Index inside the path token -> whole token.
        assert_eq!(token_at_column(row, 8).as_deref(), Some("src/lib/foo.ts"));
        // Index on the first word.
        assert_eq!(token_at_column(row, 0).as_deref(), Some("wrote"));
        // Index on whitespace -> nothing.
        assert_eq!(token_at_column(row, 5), None);
        // Index past the end -> nothing.
        assert_eq!(token_at_column(row, 999), None);
    }

    #[test]
    fn build_editor_argv_is_injection_safe() {
        // Column present.
        assert_eq!(
            build_editor_argv("zed {path}:{line}:{col}", "/a/b.rs", 42, Some(9)),
            Some(vec!["zed".into(), "/a/b.rs:42:9".into()])
        );
        // No column -> trailing ":{col}" is dropped, not left dangling.
        assert_eq!(
            build_editor_argv("zed {path}:{line}:{col}", "/a/b.rs", 42, None),
            Some(vec!["zed".into(), "/a/b.rs:42".into()])
        );
        // Path with a space stays a SINGLE argv element.
        assert_eq!(
            build_editor_argv("zed {path}:{line}:{col}", "/a b/c.rs", 1, Some(2)),
            Some(vec!["zed".into(), "/a b/c.rs:1:2".into()])
        );
        // Shell metacharacters stay inside one argument (never re-split, never eval).
        assert_eq!(
            build_editor_argv("zed {path}:{line}", "/tmp/$(rm -rf ~).rs", 3, None),
            Some(vec!["zed".into(), "/tmp/$(rm -rf ~).rs:3".into()])
        );
        // Empty template -> no command.
        assert_eq!(build_editor_argv("   ", "/a.rs", 1, None), None);
    }

    /// Set up a real git repo on disk with a single empty commit and a stable
    /// label; the helper is used by the linked-worktree branch tests below.
    struct GitRepoFixture {
        root: PathBuf,
    }

    impl GitRepoFixture {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "taarof-read-git-branch-{label}-{nonce}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            run_git_quiet(
                root.parent().unwrap(),
                &["init", "-b", "main", root.to_str().unwrap()],
            );
            run_git_quiet(&root, &["config", "user.email", "taarof@example.com"]);
            run_git_quiet(&root, &["config", "user.name", "taarof"]);
            std::fs::write(root.join("README.md"), "hello\n").unwrap();
            run_git_quiet(&root, &["add", "README.md"]);
            run_git_quiet(&root, &["commit", "-m", "init"]);
            Self { root }
        }
    }

    impl Drop for GitRepoFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run_git_quiet(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git binary should run for the read_git_branch fixture");
        assert!(
            output.status.success(),
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn read_git_branch_returns_main_branch_for_a_plain_checkout() {
        let repo = GitRepoFixture::new("plain-checkout");
        let branch = read_git_branch(repo.root.to_str().unwrap());
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn read_git_branch_returns_worktree_branch_for_linked_worktree() {
        let repo = GitRepoFixture::new("linked-worktree-branch");
        let worktree_branch = "feature/read-git-branch";
        // Use a unique worktree parent dir to avoid clashing with previous
        // test runs that might have left a sibling directory in place.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let wt_parent = std::env::temp_dir().join(format!(
            "taarof-read-git-branch-wt-parent-{nonce}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&wt_parent).unwrap();
        let wt_relative = wt_parent.join("wt-read-git-branch");
        let wt_relative_str = wt_relative.to_str().unwrap().to_string();
        run_git_quiet(
            &repo.root,
            &["worktree", "add", "-b", worktree_branch, &wt_relative_str],
        );
        assert!(wt_relative.exists());

        let from_worktree = read_git_branch(wt_relative.to_str().unwrap());
        let from_main = read_git_branch(repo.root.to_str().unwrap());

        // Even when the main checkout has linked worktrees, the branch read
        // for the main checkout must still report the main branch — and not
        // any of the linked worktree branches. The historical bug was that
        // reading the main checkout's `.git/HEAD` blindly always worked, but
        // resolving through `git::discover` keeps the contract stable.
        let _ = std::fs::remove_dir_all(&wt_parent);

        assert_eq!(from_worktree.as_deref(), Some(worktree_branch));
        assert_eq!(from_main.as_deref(), Some("main"));
    }

    #[test]
    fn read_git_branch_returns_none_outside_a_repo() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("taarof-read-git-branch-empty-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let branch = read_git_branch(dir.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(branch.is_none());
    }
}
