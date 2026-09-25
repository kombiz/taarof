//! Broadcast input dispatch: forwarding keystrokes and text to multiple panes.

use super::*;

use std::collections::VecDeque;
use std::rc::Weak;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DispatchTargetId {
    pub(super) tab_id: u32,
    pub(super) pane_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DispatchTabSnapshot {
    pub(super) workspace_id: u32,
    pub(super) tab_id: u32,
    pub(super) focused_pane_id: u32,
    pub(super) pane_ids: Vec<u32>,
}

pub(super) fn resolve_dispatch_target_ids(
    active_workspace_id: u32,
    active_tab_id: Option<u32>,
    broadcast_scope: Option<BroadcastScope>,
    tabs: &[DispatchTabSnapshot],
) -> Vec<DispatchTargetId> {
    match broadcast_scope {
        Some(BroadcastScope::Tab) => {
            let Some(active_tab_id) = active_tab_id else {
                return Vec::new();
            };
            tabs.iter()
                .find(|tab| tab.tab_id == active_tab_id)
                .into_iter()
                .flat_map(|tab| {
                    tab.pane_ids
                        .iter()
                        .copied()
                        .map(|pane_id| DispatchTargetId {
                            tab_id: tab.tab_id,
                            pane_id,
                        })
                })
                .collect()
        }
        Some(BroadcastScope::Workspace) => tabs
            .iter()
            .filter(|tab| tab.workspace_id == active_workspace_id)
            .flat_map(|tab| {
                tab.pane_ids
                    .iter()
                    .copied()
                    .map(|pane_id| DispatchTargetId {
                        tab_id: tab.tab_id,
                        pane_id,
                    })
            })
            .collect(),
        None => {
            let Some(active_tab_id) = active_tab_id else {
                return Vec::new();
            };
            tabs.iter()
                .find(|tab| tab.tab_id == active_tab_id)
                .map(|tab| {
                    vec![DispatchTargetId {
                        tab_id: tab.tab_id,
                        pane_id: tab.focused_pane_id,
                    }]
                })
                .unwrap_or_default()
        }
    }
}

pub(super) fn collect_dispatch_snapshots(state: &AppState) -> Vec<DispatchTabSnapshot> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().map(|tab| DispatchTabSnapshot {
                workspace_id: workspace.id,
                tab_id: tab.id,
                focused_pane_id: tab.focused_pane_id,
                pane_ids: tab
                    .panes
                    .leaves()
                    .into_iter()
                    .map(|leaf| leaf.pane_id)
                    .collect(),
            })
        })
        .collect()
}

pub(super) fn current_dispatch_target_ids(state: &AppState) -> Vec<DispatchTargetId> {
    resolve_dispatch_target_ids(
        state.active_workspace,
        state.active_tab().map(|tab| tab.id),
        state.broadcast_scope(),
        &collect_dispatch_snapshots(state),
    )
}

pub(super) fn current_scope_terminals(state: &Rc<RefCell<AppState>>) -> Vec<vte::Terminal> {
    let st = state.borrow();
    let target_ids = current_dispatch_target_ids(&st);
    target_ids
        .into_iter()
        .filter_map(|target| {
            let (_, tab) = st.find_tab(target.tab_id)?;
            tab.panes
                .leaf(target.pane_id)
                .map(|leaf| leaf.terminal.clone())
        })
        .collect()
}

pub(super) fn broadcast_peer_terminals(
    state: &Rc<RefCell<AppState>>,
    source_terminal: &vte::Terminal,
) -> Vec<vte::Terminal> {
    current_scope_terminals(state)
        .into_iter()
        .filter(|terminal| terminal.as_ptr() != source_terminal.as_ptr())
        .collect()
}

pub(super) fn feed_terminals(terminals: Vec<vte::Terminal>, bytes: &[u8]) -> usize {
    for terminal in &terminals {
        terminal.feed_child(bytes);
    }
    terminals.len()
}

pub(super) struct BroadcastForwardingReset<'a> {
    pub(super) active: &'a Cell<bool>,
}

impl Drop for BroadcastForwardingReset<'_> {
    fn drop(&mut self) {
        self.active.set(false);
    }
}

pub(super) fn with_broadcast_forwarding_guard<T>(forward: impl FnOnce() -> T) -> Option<T> {
    BROADCAST_FORWARDING_ACTIVE.with(|active| {
        if active.replace(true) {
            return None;
        }
        let _reset = BroadcastForwardingReset { active };
        Some(forward())
    })
}

pub(super) fn forward_broadcast_bytes(terminals: Vec<vte::Terminal>, bytes: &[u8]) -> usize {
    with_broadcast_forwarding_guard(|| feed_terminals(terminals, bytes)).unwrap_or(0)
}

pub(super) fn should_broadcast_commit(text: &str, broadcast_scope: Option<BroadcastScope>) -> bool {
    !text.is_empty() && broadcast_scope.is_some()
}

#[cfg(any(test, feature = "harness", debug_assertions))]
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastCommitHarnessResult {
    pub forward_entries: usize,
    pub peer_lookup_count: usize,
    pub suppressed_reentrant_entries: usize,
    pub received_payloads: Vec<Vec<String>>,
}

#[cfg(any(test, feature = "harness", debug_assertions))]
#[doc(hidden)]
pub fn exercise_broadcast_commit_reentrancy_harness(
    terminal_count: usize,
    broadcast_scope: Option<BroadcastScope>,
    text: &str,
) -> BroadcastCommitHarnessResult {
    assert!(
        terminal_count >= 2,
        "broadcast harness requires at least two synthetic terminals"
    );

    struct Harness {
        terminal_count: usize,
        broadcast_scope: Option<BroadcastScope>,
        result: BroadcastCommitHarnessResult,
    }

    fn simulate_commit(harness: &mut Harness, source_terminal: usize, text: &str) {
        if !should_broadcast_commit(text, harness.broadcast_scope) {
            return;
        }

        if with_broadcast_forwarding_guard(|| {
            harness.result.forward_entries += 1;
            harness.result.peer_lookup_count += 1;
            for peer_terminal in 0..harness.terminal_count {
                if peer_terminal == source_terminal {
                    continue;
                }
                harness.result.received_payloads[peer_terminal].push(text.to_string());
                simulate_commit(harness, peer_terminal, text);
            }
        })
        .is_none()
        {
            harness.result.suppressed_reentrant_entries += 1;
        }
    }

    let mut harness = Harness {
        terminal_count,
        broadcast_scope,
        result: BroadcastCommitHarnessResult {
            forward_entries: 0,
            peer_lookup_count: 0,
            suppressed_reentrant_entries: 0,
            received_payloads: vec![Vec::new(); terminal_count],
        },
    };
    simulate_commit(&mut harness, 0, text);
    harness.result
}

/// Reserved controlled broadcast entry point. Task affordances deliberately do
/// not call this: they launch isolated task tabs through `task_launch`.
#[allow(dead_code)]
pub(crate) fn send_text_to_active_scope(state: &Rc<RefCell<AppState>>, bytes: &[u8]) -> usize {
    if BROADCAST_FORWARDING_ACTIVE.with(|a| a.get()) {
        eprintln!(
            "taarof: send_text_to_active_scope: suppressed bytes={} reason=broadcast-forwarding-active",
            bytes.len()
        );
        return 0;
    }
    forward_broadcast_bytes(current_scope_terminals(state), bytes)
}

pub(crate) fn paste_clipboard_to_active_scope(state: &Rc<RefCell<AppState>>) -> usize {
    let terminals = current_scope_terminals(state);
    for terminal in &terminals {
        terminal.paste_clipboard();
    }
    terminals.len()
}

/// Whether "send to pane" delivers the payload as-is (Insert) or appends a
/// single newline so the target shell/agent executes it (Run).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendToPaneMode {
    Insert,
    Run,
}

/// Build the exact byte payload to deliver to a target pane for "send to pane".
///
/// Source precedence: a non-empty `selection` wins; otherwise a non-empty
/// `clipboard`. Trailing `\r`/`\n` are trimmed from the chosen source so the
/// caller decides newline semantics. In `Insert` mode the text lands with NO
/// trailing newline, so it sits in the target agent's composer without
/// executing; in `Run` mode exactly one `\n` is appended. Returns `None` when
/// neither source has usable (non-whitespace-only-newline) text.
pub fn build_send_to_pane_payload(
    selection: Option<&str>,
    clipboard: Option<&str>,
    mode: SendToPaneMode,
) -> Option<Vec<u8>> {
    let src = selection
        .filter(|s| !s.is_empty())
        .or_else(|| clipboard.filter(|s| !s.is_empty()))?;
    let base = src.trim_end_matches(['\r', '\n']);
    if base.is_empty() {
        return None;
    }
    match mode {
        SendToPaneMode::Insert => Some(base.as_bytes().to_vec()),
        SendToPaneMode::Run => Some(format!("{base}\n").into_bytes()),
    }
}

/// A resolved delivery endpoint for a pane: either a live VTE terminal or a
/// tmux backing (headless or explicit). Mirrors socket.rs `PaneControlTarget`
/// so the GTK "send to pane" path and the socket `send-keys` path share one
/// resolver.
pub(crate) enum PaneSendTarget {
    Vte(vte::Terminal),
    Tmux(Box<crate::pane::TmuxBacking>),
}

/// Resolve a `(tab_id, pane_id)` to its delivery endpoint. A live leaf prefers
/// its tmux backing, else its VTE terminal; a headless pane resolves through its
/// tmux backing. Returns `None` when the tab or pane no longer exists. This
/// mirrors the resolution in `socket::handle_send_keys`.
pub(crate) fn resolve_pane_send_target(
    st: &AppState,
    tab_id: u32,
    pane_id: u32,
) -> Option<PaneSendTarget> {
    let (_, tab) = st.find_tab(tab_id)?;
    if let Some(leaf) = tab
        .panes
        .leaves()
        .into_iter()
        .find(|leaf| leaf.pane_id == pane_id)
    {
        Some(
            leaf.tmux_backing
                .clone()
                .map(|backing| PaneSendTarget::Tmux(Box::new(backing)))
                .unwrap_or_else(|| PaneSendTarget::Vte(leaf.terminal.clone())),
        )
    } else {
        st.headless_pane(tab_id, pane_id)
            .and_then(|pane| pane.tmux_backing.clone())
            .map(|backing| PaneSendTarget::Tmux(Box::new(backing)))
    }
}

/// Deliver raw bytes to a pane. Resolves + clones the target under a short
/// borrow, then DROPS the borrow before feeding: `vte::Terminal::feed_child`
/// can synchronously re-enter `AppState` on this thread, which would panic if a
/// borrow were still held (mirrors `socket::handle_run_in_pane`). tmux-backed
/// panes enqueue `send-keys -l` (literal) on the bounded tmux worker, matching
/// the socket path without waiting on GTK.
pub(crate) fn send_bytes_to_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    bytes: &[u8],
) -> Result<(), String> {
    let target = {
        let st = state.borrow();
        resolve_pane_send_target(&st, tab_id, pane_id)
            .ok_or_else(|| "target pane no longer exists".to_string())?
    }; // borrow released here — safe to feed_child now.
    match target {
        PaneSendTarget::Vte(terminal) => {
            terminal.feed_child(bytes);
            Ok(())
        }
        PaneSendTarget::Tmux(backing) => {
            let payload = String::from_utf8_lossy(bytes).into_owned();
            let worker = crate::tmux::default_worker();
            glib::spawn_future_local(async move {
                let error = match worker
                    .submit(
                        vec![crate::tmux::send_keys_backing_command(&backing, &payload)],
                        std::time::Duration::from_secs(10),
                    )
                    .await
                {
                    Ok(completion) => completion
                        .outcomes()
                        .first()
                        .and_then(|outcome| outcome.result().err()),
                    Err(error) => Some(error),
                };
                if let Some(error) = error {
                    crate::show_error_toast(&format!("tmux send-keys failed: {error}"));
                }
            });
            Ok(())
        }
    }
}

/// The active pane's current selection text, if any. Returns `None` when there
/// is no active terminal or nothing is selected.
pub(crate) fn active_selection_text(state: &Rc<RefCell<AppState>>) -> Option<String> {
    let terminal = crate::get_active_terminal(state)?;
    if !terminal.has_selection() {
        return None;
    }
    terminal
        .text_selected(vte::Format::Text)
        .map(|text| text.to_string())
}

// ── Clipboard history "kill-ring" ────────────────────────────────────────────
//
// A bounded, in-memory, workspace-global ring of taarof-initiated copies.
// In-memory only: it starts empty on every launch and is never persisted, so
// secrets that pass through the clipboard never touch disk (EXAMPLE-88).

/// One recorded copy: the text, a human-readable source-pane label captured at
/// record time, and when it was recorded. `at` is read only via `Debug`.
#[derive(Clone, Debug)]
pub(crate) struct ClipboardEntry {
    pub text: String,
    pub source: String,
    // Recorded as part of the entry contract (text, source pane, timestamp).
    // Not surfaced in the picker yet; retained for ordering/labelling use.
    #[allow(dead_code)]
    pub at: SystemTime,
}

/// Newest-first bounded ring of clipboard entries. Consecutive identical copies
/// collapse to a single entry; the ring never exceeds `cap`.
struct ClipboardRing {
    entries: VecDeque<ClipboardEntry>,
    cap: usize,
}

impl ClipboardRing {
    fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Resize the ring, trimming the oldest entries when shrinking below `len`.
    fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.entries.len() > self.cap {
            self.entries.pop_back();
        }
    }

    /// Record a copy at the front (newest). Empty text is ignored; a copy
    /// identical to the current newest entry is collapsed (consecutive dedupe).
    fn record(&mut self, text: &str, source: &str) {
        if text.is_empty() {
            return;
        }
        if self.entries.front().is_some_and(|entry| entry.text == text) {
            return;
        }
        self.entries.push_front(ClipboardEntry {
            text: text.to_string(),
            source: source.to_string(),
            at: SystemTime::now(),
        });
        while self.entries.len() > self.cap {
            self.entries.pop_back();
        }
    }

    /// The entries, front = newest.
    fn entries(&self) -> &VecDeque<ClipboardEntry> {
        &self.entries
    }
}

thread_local! {
    static CLIPBOARD_RING: RefCell<ClipboardRing> = RefCell::new(ClipboardRing::new(50));
    // Weak handle to AppState so a copy can resolve its source-pane label at
    // record time without threading state through every copy call site.
    static RING_STATE: RefCell<Option<Weak<RefCell<AppState>>>> = const { RefCell::new(None) };
}

/// Install the shared AppState handle used to label copy sources. Called once
/// during `build_ui` (mirrors `set_toast_overlay`).
pub(crate) fn install_clipboard_ring_state(state: &Rc<RefCell<AppState>>) {
    RING_STATE.with(|cell| {
        *cell.borrow_mut() = Some(Rc::downgrade(state));
    });
}

/// Build a source-pane label for the currently-focused pane, reusing the exact
/// agent-instance labelling the "send to pane" picker uses so the label matches
/// what the user already sees in the sidebar.
fn focused_pane_source_label(st: &AppState) -> String {
    let Some(tab) = st.active_tab() else {
        return "pane".to_string();
    };
    let pane_id = tab.focused_pane_id;
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
    let agent_label = instances
        .iter()
        .find(|instance| instance.pane_id == pane_id)
        .map(|instance| instance.instance_label.clone())
        .unwrap_or_else(|| "shell".to_string());
    format!("{agent_label} · pane {pane_id}")
}

/// Resolve the source label for the current copy. Uses `try_borrow` so a
/// transiently-borrowed AppState never panics — a missing label is acceptable
/// for a hint.
fn active_source_label() -> String {
    RING_STATE.with(|cell| {
        let borrowed = cell.borrow();
        let Some(weak) = borrowed.as_ref() else {
            return "pane".to_string();
        };
        let Some(state) = weak.upgrade() else {
            return "pane".to_string();
        };
        let label = match state.try_borrow() {
            Ok(st) => focused_pane_source_label(&st),
            Err(_) => "pane".to_string(),
        };
        label
    })
}

/// Record a taarof-initiated copy of `text` in the clipboard history ring.
/// No-op for empty text. Reads the configured cap on every call so config
/// reloads take effect.
pub(crate) fn record_clipboard_copy(text: &str) {
    if text.is_empty() {
        return;
    }
    let cap = crate::config::terminal_config().clipboard_history_size as usize;
    let source = active_source_label();
    CLIPBOARD_RING.with(|r| {
        let mut r = r.borrow_mut();
        r.set_cap(cap);
        r.record(text, &source);
    });
}

/// Copy the VTE selection to the clipboard AND record it in the history ring.
/// Wraps the bare `copy_clipboard_format` call so every copy site is instrumented.
pub(crate) fn copy_vte_selection_and_record(terminal: &vte::Terminal) {
    terminal.copy_clipboard_format(vte::Format::Text);
    if let Some(text) = terminal.text_selected(vte::Format::Text) {
        record_clipboard_copy(&text);
    }
}

/// A newest-first snapshot of the clipboard history ring for the picker.
pub(crate) fn clipboard_ring_snapshot() -> Vec<ClipboardEntry> {
    CLIPBOARD_RING.with(|r| r.borrow().entries().iter().cloned().collect())
}

/// Re-copy a chosen history entry's text back onto the system clipboard.
pub(crate) fn copy_ring_entry_to_clipboard(text: &str) -> Result<(), String> {
    set_clipboard_text(text)
}

pub(super) fn set_clipboard_text(text: &str) -> Result<(), String> {
    let Some(display) = gdk::Display::default() else {
        let message = "Clipboard unavailable: no display is attached. Connect a display session or use a tool like xclip / wl-copy to copy text.";
        crate::show_error_toast(message);
        return Err(message.to_string());
    };
    display.clipboard().set_text(text);
    Ok(())
}

/// Capture the last `line_count` grid rows as clipboard text.
///
/// Intentionally left on VTE's native `text_range_format`, which is already
/// soft-wrap aware: `get_text` joins soft-wrapped rows and inserts a newline
/// only at real line breaks (see vte.cc `is_soft_wrapped`). Do NOT "fix" this
/// into the row-wise `capture_grid_rows` path — this is a hot copy path and the
/// whole-range call already produces logical lines. The same holds for
/// `copy_vte_selection_and_record` and `active_selection_text` above.
pub(crate) fn capture_last_terminal_lines(
    terminal: &vte::Terminal,
    line_count: u32,
) -> Result<(String, u32), String> {
    if line_count == 0 {
        return Err("recent-output line count must be greater than zero".into());
    }
    if line_count > MAX_CAPTURE_SCROLLBACK_LINES {
        return Err(format!(
            "recent-output line count exceeds max {} lines",
            MAX_CAPTURE_SCROLLBACK_LINES
        ));
    }

    let (_cursor_col, cursor_row) = terminal.cursor_position();
    let cols = terminal.column_count();
    let row_count = terminal.row_count();
    if cols <= 0 || row_count <= 0 {
        return Ok((String::new(), 0));
    }

    let start_row = (cursor_row - line_count as libc::c_long + 1).max(0);
    let (text, _len) =
        terminal.text_range_format(vte::Format::Text, start_row, 0, cursor_row, cols - 1);
    let rows = u32::try_from(cursor_row - start_row + 1).unwrap_or(0);

    Ok((text.map(|g| g.to_string()).unwrap_or_default(), rows))
}

/// One captured terminal grid row plus whether it soft-wrapped into the next
/// row. `soft_wrapped == true` means the line continued only because it reached
/// the pane's right edge, so it must be rejoined with the following row to
/// reconstruct the original logical line; `false` means the row ended with a
/// real newline (or is the final captured row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedRow {
    pub(crate) text: String,
    pub(crate) soft_wrapped: bool,
}

/// Join captured grid rows into logical lines. A soft-wrapped row concatenates
/// directly onto the next row; a hard-terminated row is followed by a newline;
/// the final row never gets a trailing newline. This mirrors VTE's own
/// `get_text` loop, which appends '\n' between rows only when the row is *not*
/// soft-wrapped (vte.cc `is_soft_wrapped`).
pub(crate) fn join_logical_lines(rows: &[CapturedRow]) -> String {
    let mut out = String::new();
    for (idx, row) in rows.iter().enumerate() {
        out.push_str(&row.text);
        let is_last = idx + 1 == rows.len();
        if !is_last && !row.soft_wrapped {
            out.push('\n');
        }
    }
    out
}

/// Extract grid rows `start_row..=end_row` (inclusive) as [`CapturedRow`]s,
/// deriving each row's soft-wrap flag.
///
/// The vte4 crate does not expose per-row wrap attributes, so the flag is
/// recovered with an adjacent-pair extraction: VTE inserts a boundary '\n'
/// between rows `r` and `r + 1` iff row `r` is *not* soft-wrapped, and grid-row
/// text itself never contains '\n', so a newline anywhere in the two-row range
/// decides the flag exactly. Joining the resulting rows with
/// [`join_logical_lines`] reproduces VTE's whole-range `text_range_format`
/// output byte-for-byte.
///
/// Costs up to two FFI text extractions per row; this runs on the socket
/// capture path only, never on the GTK copy hot path.
pub(crate) fn capture_grid_rows(
    terminal: &vte::Terminal,
    start_row: libc::c_long,
    end_row: libc::c_long,
    cols: libc::c_long,
) -> Vec<CapturedRow> {
    let mut rows = Vec::new();
    let mut r = start_row;
    while r <= end_row {
        // A single-row range never appends a trailing newline (VTE's
        // `row < end_row` branch cannot fire), so this is the row's raw text.
        let text = terminal
            .text_range_format(vte::Format::Text, r, 0, r, cols - 1)
            .0
            .map(|g| g.to_string())
            .unwrap_or_default();
        let soft_wrapped = if r < end_row {
            let pair = terminal
                .text_range_format(vte::Format::Text, r, 0, r + 1, cols - 1)
                .0
                .map(|g| g.to_string())
                .unwrap_or_default();
            !pair.contains('\n')
        } else {
            false
        };
        rows.push(CapturedRow { text, soft_wrapped });
        r += 1;
    }
    rows
}

// ── Shell-integration command blocks (OSC 133) ───────────────────────────────
//
// The helper emits standard OSC 133 plus VTE's explicit
// OSC 666;vte.shell.precmd! signal. The app records a cursor row for that
// termprop in PaneLeaf::command_marks. OSC 133 alone is not sufficient.

// When no integration is sourced the mark buffer is empty and copy
// falls back to the unchanged last-N-lines capture.

/// Inclusive row bounds of the last command's output, given recorded prompt
/// marks. Output lies strictly between the last two prompt marks — the previous
/// prompt/command row and the new prompt row are both excluded. Returns `None`
/// when there are fewer than two marks or the two marks are adjacent (no output
/// rows between them), so the caller falls back to the last-N-lines capture.
pub(crate) fn command_output_bounds(
    marks: &[libc::c_long],
) -> Option<(libc::c_long, libc::c_long)> {
    if marks.len() < 2 {
        return None;
    }
    let last = marks[marks.len() - 1];
    let prev = marks[marks.len() - 2];
    let (start, end) = (prev + 1, last - 1);
    if start > end {
        return None;
    }
    Some((start, end))
}

/// Direction for prompt-to-prompt scroll jumps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptJump {
    Previous,
    Next,
}

/// The nearest recorded prompt-marker row to jump to from `current_top` (the
/// viewport's top row). `Previous` returns the greatest mark strictly above
/// `current_top`; `Next` the least mark strictly below it. Marks are scanned
/// rather than assumed sorted, so a shell `clear` that resets the row counter
/// can't corrupt the result. Returns `None` when there is no mark in the
/// requested direction.
pub(crate) fn next_prompt_mark(
    marks: &[libc::c_long],
    current_top: libc::c_long,
    dir: PromptJump,
) -> Option<libc::c_long> {
    match dir {
        PromptJump::Previous => marks.iter().copied().filter(|&m| m < current_top).max(),
        PromptJump::Next => marks.iter().copied().filter(|&m| m > current_top).min(),
    }
}

/// Capture exactly the previous command's output block using recorded prompt
/// marks, soft-wrap aware. Reuses the same `capture_grid_rows` +
/// `join_logical_lines` path as the socket capture (not the hot GTK path).
/// Returns `(text, row_count)`, or `None` when there are too few marks (caller
/// falls back to the last-N-lines capture).
pub(crate) fn capture_command_block(
    terminal: &vte::Terminal,
    marks: &[libc::c_long],
) -> Option<(String, u32)> {
    let (start, end) = command_output_bounds(marks)?;
    let cols = terminal.column_count();
    if cols <= 0 {
        return None;
    }
    let start = start.max(0);
    if end < start {
        return None;
    }
    let rows = capture_grid_rows(terminal, start, end, cols);
    let text = join_logical_lines(&rows);
    let n = u32::try_from(end - start + 1).unwrap_or(0);
    Some((text, n))
}

pub(crate) fn copy_recent_output_to_clipboard(terminal: &vte::Terminal) -> Result<u32, String> {
    let line_count = crate::config::terminal_config().copy_recent_lines;
    let (text, rows) = capture_last_terminal_lines(terminal, line_count)?;
    set_clipboard_text(&text)?;
    record_clipboard_copy(&text);
    Ok(rows)
}

/// Copy the previous command's output for a specific pane. When prompt marks are
/// present this is an exact command-block capture (no prompt lines, no 200-line
/// truncation); otherwise it falls back to the unchanged last-N-lines capture.
/// Returns the number of rows copied.
pub(crate) fn copy_recent_output_for_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) -> Result<u32, String> {
    // Resolve + clone under a short borrow, then DROP it before touching VTE or
    // the clipboard (mirrors `send_bytes_to_pane`): capture/feed paths can
    // re-enter AppState on this thread.
    let (terminal, marks) = {
        let st = state.borrow();
        let (_, tab) = st
            .find_tab(tab_id)
            .ok_or_else(|| "target tab no longer exists".to_string())?;
        let terminal = tab
            .panes
            .focused_terminal(pane_id)
            .cloned()
            .ok_or_else(|| "target pane has no terminal".to_string())?;
        let marks = tab
            .panes
            .leaf(pane_id)
            .map(|leaf| leaf.command_marks.clone())
            .unwrap_or_default();
        (terminal, marks)
    };

    if let Some((text, rows)) = capture_command_block(&terminal, &marks) {
        set_clipboard_text(&text)?;
        record_clipboard_copy(&text);
        return Ok(rows);
    }
    copy_recent_output_to_clipboard(&terminal)
}

pub(crate) fn copy_recent_output_from_active_terminal(
    state: &Rc<RefCell<AppState>>,
) -> Result<u32, String> {
    let (tab_id, pane_id) = {
        let st = state.borrow();
        let tab = st
            .active_tab()
            .ok_or_else(|| "no active terminal pane".to_string())?;
        (tab.id, tab.focused_pane_id)
    };
    copy_recent_output_for_pane(state, tab_id, pane_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LastAgentMessageResolution {
    Transcript(String),
    Unavailable { agent: String, reason: String },
}

fn agent_display_name(agent: Option<&str>) -> String {
    match agent {
        Some("claude") => "Claude Code".to_string(),
        Some("codex") => "Codex".to_string(),
        Some("pi") => "Pi Agent".to_string(),
        Some(agent) if !agent.trim().is_empty() => agent.to_string(),
        _ => "Agent".to_string(),
    }
}

/// Pure resolver shared by Copy Last Agent Message and Relay last message.
/// It returns byte-identical source markdown when available, otherwise carries
/// the detected agent and a user-facing explanation of why no transcript
/// message was available.
pub(crate) fn resolve_last_agent_message(
    transcript: Option<&crate::agents::TranscriptState>,
    detected_agent: Option<&str>,
) -> LastAgentMessageResolution {
    if let Some(text) = transcript.and_then(|transcript| transcript.last_message.as_ref()) {
        if !text.trim().is_empty() {
            return LastAgentMessageResolution::Transcript(text.clone());
        }
    }

    let agent = transcript
        .map(|transcript| transcript.agent.as_str())
        .filter(|agent| !agent.is_empty())
        .or(detected_agent);
    let reason = match agent {
        Some("claude" | "codex" | "pi") => {
            "no unambiguous native transcript matched this pane's identity and process metadata"
        }
        Some(_) => "this agent does not expose a supported native transcript",
        None => "no running agent was detected in this pane",
    };
    LastAgentMessageResolution::Unavailable {
        agent: agent_display_name(agent),
        reason: reason.to_string(),
    }
}

/// Borrow `AppState` briefly to resolve a pane's last agent message, returning
/// an owned Option so the borrow drops before any clipboard write.
pub(crate) fn last_agent_message_for_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) -> LastAgentMessageResolution {
    let st = state.borrow();
    let detected_agent = st
        .runtime_probe
        .as_ref()
        .and_then(|snapshot| snapshot.pane_agents.get(&(tab_id, pane_id)))
        .filter(|status| status.running)
        .and_then(|status| status.agent_name.as_deref())
        .or_else(|| {
            let tab = st
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find(|tab| tab.id == tab_id)?;
            (tab.agent_running && tab.agent_pane_id.unwrap_or(tab.focused_pane_id) == pane_id)
                .then_some(tab.agent_name.as_deref())
                .flatten()
        });
    resolve_last_agent_message(st.pane_transcripts.get(&(tab_id, pane_id)), detected_agent)
}

/// User-facing message for a Copy Last Agent Message that copied nothing.
/// The action never substitutes visible rows for a transcript message: the
/// clipboard keeps whatever it held, and the toast names the action that does
/// copy the visible rows (Copy Recent Output) with its live shortcut when one
/// is installed.
pub(crate) fn last_message_unavailable_message(
    agent: &str,
    reason: &str,
    recent_output_shortcut: Option<&str>,
) -> String {
    match recent_output_shortcut {
        Some(shortcut) => format!(
            "{agent}: {reason}. Nothing was copied; use Copy Recent Output ({shortcut}) for the \
             visible rows."
        ),
        None => format!(
            "{agent}: {reason}. Nothing was copied; use Copy Recent Output for the visible rows."
        ),
    }
}

/// Copy a pane's last agent message as raw markdown when a transcript exists.
/// When no transcript matches, nothing is copied and the error names the
/// Copy Recent Output action instead, so the shortcut always means one thing.
/// Mirrors the short-borrow-then-drop discipline of
/// `copy_recent_output_for_pane`: the state borrow inside
/// `last_agent_message_for_pane` drops before the clipboard write here.
pub(crate) fn copy_last_message_for_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) -> Result<(), String> {
    match last_agent_message_for_pane(state, tab_id, pane_id) {
        LastAgentMessageResolution::Transcript(text) => {
            set_clipboard_text(&text)?;
            record_clipboard_copy(&text);
            Ok(())
        }
        LastAgentMessageResolution::Unavailable { agent, reason } => {
            let shortcut =
                crate::keybindings::label_for(crate::keybindings::Action::CopyRecentOutput);
            Err(last_message_unavailable_message(
                &agent,
                &reason,
                shortcut.as_deref(),
            ))
        }
    }
}

pub(crate) fn copy_last_message_from_active_pane(
    state: &Rc<RefCell<AppState>>,
) -> Result<(), String> {
    let (tab_id, pane_id) = {
        let st = state.borrow();
        let tab = st
            .active_tab()
            .ok_or_else(|| "no active terminal pane".to_string())?;
        (tab.id, tab.focused_pane_id)
    };
    copy_last_message_for_pane(state, tab_id, pane_id)
}

pub(crate) fn last_agent_message_for_active_pane(
    state: &Rc<RefCell<AppState>>,
) -> LastAgentMessageResolution {
    let (tab_id, pane_id) = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            return LastAgentMessageResolution::Unavailable {
                agent: "Agent".to_string(),
                reason: "no active terminal pane was found".to_string(),
            };
        };
        (tab.id, tab.focused_pane_id)
    };
    last_agent_message_for_pane(state, tab_id, pane_id)
}

pub(crate) fn broadcast_target_count(state: &Rc<RefCell<AppState>>) -> usize {
    let st = state.borrow();
    if st.broadcast_scope().is_none() {
        0
    } else {
        current_dispatch_target_ids(&st).len()
    }
}

pub(super) fn control_key_byte(key: gdk::Key) -> Option<u8> {
    let ch = key.to_unicode()?.to_ascii_uppercase();
    match ch {
        '@' | ' ' | '2' => Some(0x00),
        'A'..='Z' => Some((ch as u8) - b'A' + 1),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '/' | '7' => Some(0x1f),
        '8' => Some(0x7f),
        _ => None,
    }
}

pub(super) fn encode_broadcast_key_input(
    key: gdk::Key,
    mods: gdk::ModifierType,
) -> Option<Vec<u8>> {
    let mods = mods
        & (gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::META_MASK
            | gdk::ModifierType::SUPER_MASK);

    if mods.intersects(
        gdk::ModifierType::ALT_MASK | gdk::ModifierType::META_MASK | gdk::ModifierType::SUPER_MASK,
    ) {
        return None;
    }

    if mods.contains(gdk::ModifierType::CONTROL_MASK)
        && !mods.contains(gdk::ModifierType::SHIFT_MASK)
    {
        return control_key_byte(key).map(|byte| vec![byte]);
    }

    match key {
        gdk::Key::Return | gdk::Key::KP_Enter => Some(vec![b'\r']),
        gdk::Key::BackSpace => Some(vec![0x7f]),
        gdk::Key::Tab => Some(vec![b'\t']),
        gdk::Key::ISO_Left_Tab => Some(b"\x1b[Z".to_vec()),
        gdk::Key::Escape => Some(vec![0x1b]),
        gdk::Key::Up => Some(b"\x1b[A".to_vec()),
        gdk::Key::Down => Some(b"\x1b[B".to_vec()),
        gdk::Key::Right => Some(b"\x1b[C".to_vec()),
        gdk::Key::Left => Some(b"\x1b[D".to_vec()),
        gdk::Key::Home => Some(b"\x1b[H".to_vec()),
        gdk::Key::End => Some(b"\x1b[F".to_vec()),
        gdk::Key::Insert => Some(b"\x1b[2~".to_vec()),
        gdk::Key::Delete => Some(b"\x1b[3~".to_vec()),
        gdk::Key::Page_Up => Some(b"\x1b[5~".to_vec()),
        gdk::Key::Page_Down => Some(b"\x1b[6~".to_vec()),
        _ if key.to_unicode().is_some() => None,
        _ => None,
    }
}

pub(super) fn connect_broadcast_input(terminal: &vte::Terminal, state: &Rc<RefCell<AppState>>) {
    {
        let state = state.clone();
        terminal.connect_commit(move |terminal, text, _size| {
            if !should_broadcast_commit(text, state.borrow().broadcast_scope()) {
                return;
            }
            forward_broadcast_bytes(broadcast_peer_terminals(&state, terminal), text.as_bytes());
        });
    }

    let broadcast_controller = gtk::EventControllerKey::new();
    {
        let state = state.clone();
        let terminal = terminal.clone();
        broadcast_controller.connect_key_pressed(move |_ctrl, key, code, mods| {
            if state.borrow().broadcast_scope().is_none() {
                return glib::Propagation::Proceed;
            }
            if crate::keybindings::matches_any_shortcut(key, mods, code) {
                return glib::Propagation::Proceed;
            }
            let Some(bytes) = encode_broadcast_key_input(key, mods) else {
                return glib::Propagation::Proceed;
            };
            forward_broadcast_bytes(broadcast_peer_terminals(&state, &terminal), &bytes);
            glib::Propagation::Proceed
        });
    }
    terminal.add_controller(broadcast_controller);
}

#[cfg(test)]
mod tests {
    use super::{
        build_send_to_pane_payload, command_output_bounds, last_message_unavailable_message,
        next_prompt_mark, resolve_last_agent_message, set_clipboard_text, ClipboardRing,
        LastAgentMessageResolution, PromptJump, SendToPaneMode,
    };

    #[test]
    fn test_copy_last_message_uses_transcript_markdown_verbatim() {
        // A realistic closing summary: multi-line, a wide markdown table (pipes)
        // and one very long line. The resolver must return it byte-for-byte,
        // proving no wrapping/truncation/reformatting happens on the copy path.
        let md = "## Summary\n\nDone. Here is what changed:\n\n\
            | File | Change | Lines |\n\
            | --- | --- | --- |\n\
            | broadcast.rs | added resolver | +40 |\n\
            | palette.rs | relay picker | +25 |\n\n\
            Next steps: run the full suite, then wire the sidebar row, then verify \
            in the kasm container that the toast fallback reads correctly on Codex \
            panes which resolve no transcript at all.";
        let state = crate::agents::TranscriptState {
            last_message: Some(md.to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_last_agent_message(Some(&state), Some("codex")),
            LastAgentMessageResolution::Transcript(md.to_string())
        );
    }

    #[test]
    fn test_last_message_unavailable_message_names_copy_recent_output() {
        // P-002: when no transcript matches, the action copies nothing and the
        // toast must say so and name the action that copies the visible rows,
        // with its live shortcut when one is installed.
        let with_shortcut = last_message_unavailable_message(
            "Codex",
            "no unambiguous native transcript matched this pane's identity and process metadata",
            Some("Ctrl+Shift+O"),
        );
        assert_eq!(
            with_shortcut,
            "Codex: no unambiguous native transcript matched this pane's identity and process \
             metadata. Nothing was copied; use Copy Recent Output (Ctrl+Shift+O) for the \
             visible rows."
        );
        let without_shortcut = last_message_unavailable_message(
            "Agent",
            "no running agent was detected in this pane",
            None,
        );
        assert_eq!(
            without_shortcut,
            "Agent: no running agent was detected in this pane. Nothing was copied; use Copy \
             Recent Output for the visible rows."
        );
    }

    #[test]
    fn test_copy_last_message_reports_unavailable_instead_of_guessing() {
        // No transcript carries agent-specific provenance for the toast.
        assert_eq!(
            resolve_last_agent_message(None, Some("codex")),
            LastAgentMessageResolution::Unavailable {
                agent: "Codex".to_string(),
                reason: "no unambiguous native transcript matched this pane's identity and process metadata".to_string(),
            }
        );
        // Whitespace-only last_message is treated as unavailable.
        let whitespace = crate::agents::TranscriptState {
            agent: "pi".to_string(),
            last_message: Some("   \n\t".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_last_agent_message(Some(&whitespace), Some("codex")),
            LastAgentMessageResolution::Unavailable {
                agent: "Pi Agent".to_string(),
                reason: "no unambiguous native transcript matched this pane's identity and process metadata".to_string(),
            }
        );
    }

    #[test]
    fn test_command_block_capture_bounds_output_between_prompt_marks() {
        // Two prompt marks: the command's output is the rows strictly between
        // them (both the prior prompt/command row and the new prompt row are
        // excluded), so marks at rows 10 and 25 bound output rows 11..=24.
        assert_eq!(command_output_bounds(&[10, 25]), Some((11, 24)));

        // With a longer history only the last two marks matter — the output of
        // the *previous* command, not the whole session.
        assert_eq!(command_output_bounds(&[3, 10, 25]), Some((11, 24)));
    }

    #[test]
    fn test_command_block_capture_falls_back_without_marks() {
        // Fewer than two marks → no exact bounds → caller falls back to the
        // last-N-lines capture.
        assert_eq!(command_output_bounds(&[]), None);
        assert_eq!(command_output_bounds(&[10]), None);

        // Adjacent prompts (an empty command with no output rows between them)
        // also yield no block, so the caller falls back rather than copying an
        // empty selection.
        assert_eq!(command_output_bounds(&[10, 11]), None);
    }

    #[test]
    fn next_prompt_mark_scans_without_sort_assumption() {
        // Marks are intentionally unsorted (a shell `clear` can reset rows).
        let marks = [5, 12, 30, 20];
        assert_eq!(next_prompt_mark(&marks, 25, PromptJump::Previous), Some(20));
        assert_eq!(next_prompt_mark(&marks, 25, PromptJump::Next), Some(30));
        // Nothing above the earliest / below the latest mark.
        assert_eq!(next_prompt_mark(&marks, 5, PromptJump::Previous), None);
        assert_eq!(next_prompt_mark(&marks, 30, PromptJump::Next), None);
        // A viewport exactly on a mark jumps past it, never onto it.
        assert_eq!(next_prompt_mark(&marks, 12, PromptJump::Previous), Some(5));
        assert_eq!(next_prompt_mark(&marks, 12, PromptJump::Next), Some(20));
    }

    #[test]
    fn test_clipboard_ring_dedupes_consecutive_and_respects_cap() {
        let texts = |ring: &ClipboardRing| -> Vec<String> {
            ring.entries().iter().map(|e| e.text.clone()).collect()
        };

        let mut ring = ClipboardRing::new(3);

        // Consecutive identical copies collapse to one entry.
        ring.record("a", "pane 1");
        ring.record("a", "pane 1");
        assert_eq!(ring.entries().len(), 1);

        // A non-consecutive repeat is a distinct entry; newest is at the front.
        ring.record("b", "pane 1");
        ring.record("a", "pane 1");
        assert_eq!(ring.entries().len(), 3);
        assert_eq!(texts(&ring), vec!["a", "b", "a"]);

        // Exceeding the cap drops the oldest (back) entry.
        ring.record("c", "pane 1");
        assert_eq!(ring.entries().len(), 3);
        assert_eq!(texts(&ring), vec!["c", "a", "b"]);

        // Empty text is never recorded.
        ring.record("", "pane 1");
        assert_eq!(ring.entries().len(), 3);

        // Shrinking the cap trims oldest entries, keeping the newest.
        ring.set_cap(1);
        assert_eq!(ring.entries().len(), 1);
        assert_eq!(ring.entries().front().unwrap().text, "c");
    }

    #[test]
    fn send_to_pane_insert_never_appends_a_newline() {
        let payload =
            build_send_to_pane_payload(Some("echo hi"), Some("ignored"), SendToPaneMode::Insert)
                .expect("selection should yield a payload");
        assert_eq!(payload, b"echo hi");
        assert!(
            !payload.ends_with(b"\n"),
            "Insert mode must not append a newline"
        );
    }

    #[test]
    fn send_to_pane_run_appends_exactly_one_newline() {
        assert_eq!(
            build_send_to_pane_payload(Some("echo hi"), None, SendToPaneMode::Run).unwrap(),
            b"echo hi\n"
        );
        assert_eq!(
            build_send_to_pane_payload(Some("echo hi\n\n\n"), None, SendToPaneMode::Run).unwrap(),
            b"echo hi\n"
        );
    }

    #[test]
    fn send_to_pane_selection_wins_over_clipboard_and_rejects_empty() {
        assert_eq!(
            build_send_to_pane_payload(Some("sel"), Some("clip"), SendToPaneMode::Insert).unwrap(),
            b"sel"
        );
        assert_eq!(
            build_send_to_pane_payload(None, Some("clip"), SendToPaneMode::Insert).unwrap(),
            b"clip"
        );
        assert!(build_send_to_pane_payload(None, None, SendToPaneMode::Insert).is_none());
        assert!(build_send_to_pane_payload(Some(""), Some(""), SendToPaneMode::Insert).is_none());
    }

    #[test]
    fn clipboard_no_display_path_surfaces_a_friendly_error() {
        // When there is no default display the helper must return a clear,
        // user-facing Err that mentions the clipboard and gives a hint. The
        // actual show_error_toast call is a no-op under `cargo test` (the
        // toast overlay thread-local is unset) but the message still has to
        // be fit for the user to read.
        if gdk::Display::default().is_some() {
            // CI/headless only — skip when a real display is available so the
            // test does not poke the user's clipboard.
            return;
        }
        let err = set_clipboard_text("hello world").expect_err("no display should error");
        assert!(
            err.contains("Clipboard"),
            "error must name the clipboard: {err}"
        );
        assert!(
            err.contains("display"),
            "error must mention the missing display: {err}"
        );
        assert!(
            err.contains("xclip") || err.contains("wl-copy"),
            "error must suggest an alternative tool: {err}"
        );
    }
}

#[cfg(test)]
mod logical_lines {
    use super::{join_logical_lines, CapturedRow};

    fn row(text: &str, soft_wrapped: bool) -> CapturedRow {
        CapturedRow {
            text: text.to_string(),
            soft_wrapped,
        }
    }

    #[test]
    fn test_logical_line_capture_joins_soft_wrapped_rows() {
        // A line the terminal soft-wrapped mid-phrase rejoins into one logical
        // line, with no newline at the wrap boundary.
        let rows = vec![row("the quick brown ", true), row("fox", false)];
        assert_eq!(join_logical_lines(&rows), "the quick brown fox");

        // A markdown table wider than the pane: the first grid row soft-wrapped
        // onto the second (together they are one table row), the third is a
        // separately-terminated table row.
        let table = vec![
            row("| a | b | c ", true),
            row("| d |", false),
            row("| e | f | g | h |", false),
        ];
        assert_eq!(
            join_logical_lines(&table),
            "| a | b | c | d |\n| e | f | g | h |"
        );
    }

    #[test]
    fn test_logical_line_capture_preserves_hard_newlines() {
        // Two genuinely separate lines keep their newline boundary.
        let rows = vec![row("line one", false), row("line two", false)];
        assert_eq!(join_logical_lines(&rows), "line one\nline two");

        // The final row never gets a trailing newline, even when it is the only
        // captured row.
        let single = vec![row("only line", false)];
        assert_eq!(join_logical_lines(&single), "only line");
        assert!(!join_logical_lines(&single).ends_with('\n'));
    }
}
