// taarof-app/src/pane.rs

use glib::prelude::Cast;
use gtk::prelude::WidgetExt;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::probe::ProbeSnapshot;
use crate::tmux::TmuxTarget;

/// Optional tmux session backing for a pane.
#[derive(Clone, Debug)]
pub struct TmuxBacking {
    pub session_name: String,
    pub target: TmuxTarget,
    /// Exact generation this pane was authorized to attach. Restored panes
    /// keep the saved value even before (or after a failed) live probe so
    /// cleanup cannot acquire authority over a same-named replacement.
    pub expected_generation: Option<crate::session::SavedTmuxIdentity>,
    pub pane_info: ProbeSnapshot<crate::tmux::TmuxPaneInfo>,
}

impl TmuxBacking {
    pub fn authoritative_generation(&self) -> Option<crate::session::SavedTmuxIdentity> {
        self.expected_generation.clone().or_else(|| {
            let info = self.pane_info.value()?;
            Some(crate::session::SavedTmuxIdentity {
                session_id: info.session_id.clone(),
                session_created: info.session_created,
                continuity_id: info.continuity_id.clone()?,
            })
        })
    }

    pub fn same_execution_target(&self, other: &Self) -> bool {
        self.session_name == other.session_name
            && self.target == other.target
            && self.authoritative_generation() == other.authoritative_generation()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDirection {
    Horizontal, // top/bottom
    Vertical,   // side-by-side
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitChild {
    First,
    Second,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneNavigationDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone)]
pub struct PaneSplitPathEntry {
    pub widget: gtk::Paned,
    pub target_child: SplitChild,
}

/// Threshold: if no terminal output for this long, consider SSH pane idle.
const SSH_IDLE_THRESHOLD: Duration = Duration::from_secs(3);

/// Maximum prompt-marker rows retained per pane. Older marks are dropped from
/// the front once this cap is exceeded, so a long-lived pane can't grow the
/// buffer without bound.
const MAX_COMMAND_MARKS: usize = 1000;

#[derive(Clone, Debug, Default)]
pub struct PaneLocationState {
    pub cwd: Option<String>,
    pub cwd_host: Option<String>,
    pub updated_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct PaneProcessState {
    /// Whether the pane root has a child in the local process tree. This is
    /// unknowable for commands running beyond an SSH client; API projections
    /// therefore serialize it as null whenever `remote_shell` is true or an
    /// authoritative remote tmux target is present.
    pub has_child_process: bool,
    pub remote_shell: bool,
    pub ssh_command: Option<Vec<String>>,
    pub updated_at_unix_ms: Option<u64>,
}

pub struct PaneLeaf {
    pub pane_id: u32,
    /// Stable identity for session work-ledger continuity. Runtime pane ids
    /// may be renumbered when a sparse split tree is restored.
    pub work_origin: String,
    pub container: gtk::Box,
    pub terminal: vte::Terminal,
    pub shell_pid: Option<i32>,
    pub was_busy: bool,
    pub launch_command: Option<String>,
    /// Shared timestamp updated by VTE `contents-changed` signal.
    /// Used for output-activity-based idle detection on SSH panes.
    pub output_tracker: Rc<Cell<Option<Instant>>>,
    pub tmux_backing: Option<TmuxBacking>,
    pub location_state: PaneLocationState,
    pub process_state: PaneProcessState,
    pub current_task: Option<crate::task_binding::PaneTaskBinding>,
    /// Last persisted agent identity for this pane. Keep it across bare-shell
    /// autosaves after restore so the opt-in resume action survives another
    /// application restart; a newly detected live session supersedes it.
    pub restored_agent_session: Option<crate::session::SavedAgentSession>,
    /// Opt-in command offered after restoring a non-tmux agent pane.
    pub agent_resume: Option<crate::terminal::AgentResumeOffer>,
    /// Cursor rows recorded on each shell prompt marker (OSC 133;A, surfaced by
    /// VTE as the `vte.shell.precmd` termprop). Feeds exact "copy last command
    /// output" and jump-to-prompt scrolling. Empty when no shell integration is
    /// sourced, in which case callers fall back to the last-N-lines capture.
    pub command_marks: Vec<libc::c_long>,
    /// The broker attachment owning this pane's child PTY, when the pane was
    /// spawned through the PTY broker. Dropping it (on pane/tab close) kills the
    /// child and tears down the conduit relays. `None` for headless/stub panes.
    pub(crate) broker: Option<Rc<crate::terminal::BrokerHandle>>,
}

pub(crate) fn new_pane_work_origin() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        bytes =
            (now ^ ((u128::from(std::process::id())) << 64) ^ u128::from(counter)).to_le_bytes();
    }
    let mut rendered = String::with_capacity(37);
    rendered.push_str("pane-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

impl PaneLeaf {
    /// Record a prompt-marker cursor row. Consecutive duplicates are ignored (a
    /// redraw can re-fire the termprop at the same row, mirroring the clipboard
    /// ring's consecutive-dedupe); the buffer is trimmed from the front once it
    /// exceeds [`MAX_COMMAND_MARKS`].
    pub fn push_command_mark(&mut self, row: libc::c_long) {
        if self.command_marks.last() == Some(&row) {
            return;
        }
        self.command_marks.push(row);
        if self.command_marks.len() > MAX_COMMAND_MARKS {
            let excess = self.command_marks.len() - MAX_COMMAND_MARKS;
            self.command_marks.drain(0..excess);
        }
    }

    pub fn local_cwd(&self) -> Option<String> {
        self.location_state
            .cwd
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty() && self.location_state.cwd_host.is_none())
            .cloned()
    }

    pub fn saved_cwd(&self) -> Option<String> {
        let cwd = self.location_state.cwd.as_ref()?.trim();
        if cwd.is_empty() {
            return None;
        }

        self.location_state
            .cwd_host
            .as_ref()
            .map(|host| format!("file://{host}{cwd}"))
            .or_else(|| Some(cwd.to_string()))
    }

    pub fn ssh_command(&self) -> Option<Vec<String>> {
        self.process_state.ssh_command.clone()
    }

    pub fn update_location_cache(&mut self, cwd: Option<String>, cwd_host: Option<String>) {
        self.location_state = PaneLocationState {
            cwd,
            cwd_host,
            updated_at_unix_ms: Some(unix_time_ms()),
        };
    }

    pub fn update_process_state(&mut self, process_state: PaneProcessState) {
        self.process_state = process_state;
    }

    pub fn update_probe_cache(&mut self, has_child_process: bool, remote_shell: bool) {
        self.process_state = PaneProcessState {
            has_child_process,
            remote_shell,
            ssh_command: None,
            updated_at_unix_ms: Some(unix_time_ms()),
        };
    }
}

#[allow(clippy::large_enum_variant)] // Keeping PaneLeaf inline avoids pervasive boxing churn across pane-tree mutation paths.
pub enum PaneNode {
    Leaf(PaneLeaf),
    Split {
        direction: SplitDirection,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
        widget: gtk::Paned,
    },
    /// Headless stub used by state-only tests and smoke automation.
    Stub {
        pane_id: u32,
    },
    /// Placeholder for non-terminal tabs (e.g. Dashboard) that have no VTE panes.
    Empty,
}

impl PaneNode {
    /// DFS collect all shell PIDs.
    pub fn collect_pids(&self) -> Vec<i32> {
        match self {
            PaneNode::Leaf(leaf) => leaf.shell_pid.into_iter().collect(),
            PaneNode::Split { first, second, .. } => {
                let mut pids = first.collect_pids();
                pids.extend(second.collect_pids());
                pids
            }
            PaneNode::Stub { .. } => Vec::new(),
            PaneNode::Empty => Vec::new(),
        }
    }

    /// Find terminal by pane_id.
    pub fn focused_terminal(&self, pane_id: u32) -> Option<&vte::Terminal> {
        match self {
            PaneNode::Leaf(leaf) => {
                if leaf.pane_id == pane_id {
                    Some(&leaf.terminal)
                } else {
                    // Fallback: return this leaf's terminal if it's the only option
                    Some(&leaf.terminal)
                }
            }
            PaneNode::Split { first, second, .. } => {
                if first.contains_pane(pane_id) {
                    first.focused_terminal(pane_id)
                } else if second.contains_pane(pane_id) {
                    second.focused_terminal(pane_id)
                } else {
                    // Fallback to first leaf
                    self.first_leaf().map(|leaf| &leaf.terminal)
                }
            }
            PaneNode::Stub { .. } => None,
            PaneNode::Empty => None,
        }
    }

    /// Mutable leaf by pane_id.
    pub fn leaf_mut(&mut self, pane_id: u32) -> Option<&mut PaneLeaf> {
        match self {
            PaneNode::Leaf(leaf) if leaf.pane_id == pane_id => Some(leaf),
            PaneNode::Split { first, second, .. } => {
                if let Some(leaf) = first.leaf_mut(pane_id) {
                    Some(leaf)
                } else {
                    second.leaf_mut(pane_id)
                }
            }
            _ => None,
        }
    }

    /// Immutable leaf by pane_id.
    pub fn leaf(&self, pane_id: u32) -> Option<&PaneLeaf> {
        match self {
            PaneNode::Leaf(leaf) if leaf.pane_id == pane_id => Some(leaf),
            PaneNode::Split { first, second, .. } => {
                if let Some(leaf) = first.leaf(pane_id) {
                    Some(leaf)
                } else {
                    second.leaf(pane_id)
                }
            }
            _ => None,
        }
    }

    /// All leaves in DFS order.
    pub fn leaves(&self) -> Vec<&PaneLeaf> {
        match self {
            PaneNode::Leaf(leaf) => vec![leaf],
            PaneNode::Split { first, second, .. } => {
                let mut leaves = first.leaves();
                leaves.extend(second.leaves());
                leaves
            }
            PaneNode::Stub { .. } => Vec::new(),
            PaneNode::Empty => Vec::new(),
        }
    }

    /// Mutable leaves in DFS order.
    pub fn leaves_mut(&mut self) -> Vec<&mut PaneLeaf> {
        match self {
            PaneNode::Leaf(leaf) => vec![leaf],
            PaneNode::Split { first, second, .. } => {
                let mut leaves = first.leaves_mut();
                leaves.extend(second.leaves_mut());
                leaves
            }
            PaneNode::Stub { .. } => Vec::new(),
            PaneNode::Empty => Vec::new(),
        }
    }

    /// True if any leaf has a busy shell.
    pub fn any_busy(&self) -> bool {
        match self {
            PaneNode::Leaf(leaf) => {
                if leaf.process_state.remote_shell {
                    leaf.output_tracker
                        .get()
                        .is_some_and(|t| t.elapsed() < SSH_IDLE_THRESHOLD)
                } else {
                    leaf.process_state.has_child_process
                }
            }
            PaneNode::Split { first, second, .. } => first.any_busy() || second.any_busy(),
            PaneNode::Stub { .. } => false,
            PaneNode::Empty => false,
        }
    }

    /// True if any leaf currently looks like an SSH/mosh session.
    pub fn any_remote_shell(&self) -> bool {
        self.leaves()
            .into_iter()
            .any(|leaf| leaf.process_state.remote_shell)
    }

    /// True if the target leaf currently looks like an SSH/mosh session.
    pub fn pane_is_remote_shell(&self, pane_id: u32) -> bool {
        self.leaf(pane_id)
            .is_some_and(|leaf| leaf.process_state.remote_shell)
    }

    /// Update was_busy flags for all leaves. Returns true if any leaf
    /// transitioned from busy to idle.
    ///
    /// For local panes: uses process-tree inspection (shell has child processes).
    /// For SSH panes: uses terminal output activity (no output for SSH_IDLE_THRESHOLD).
    pub fn update_busy_transitions(&mut self) -> bool {
        match self {
            PaneNode::Leaf(leaf) => {
                let currently_busy = if leaf.process_state.remote_shell {
                    // SSH pane: "busy" = terminal had output recently
                    leaf.output_tracker
                        .get()
                        .is_some_and(|t| t.elapsed() < SSH_IDLE_THRESHOLD)
                } else {
                    // Local pane: "busy" = shell has child processes
                    leaf.process_state.has_child_process
                };

                let transitioned = leaf.was_busy && !currently_busy;
                leaf.was_busy = currently_busy;
                transitioned
            }
            PaneNode::Split { first, second, .. } => {
                let t1 = first.update_busy_transitions();
                let t2 = second.update_busy_transitions();
                t1 || t2
            }
            PaneNode::Stub { .. } => false,
            PaneNode::Empty => false,
        }
    }

    /// True if this node is a Split variant.
    pub fn is_split(&self) -> bool {
        matches!(self, PaneNode::Split { .. })
    }

    /// Count all leaves in the tree.
    pub fn leaf_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
            PaneNode::Stub { .. } => 1,
            PaneNode::Empty => 0,
        }
    }

    /// First live leaf in DFS order (for fallback).
    ///
    /// Non-terminal tabs and headless state-only trees do not contain a live
    /// [`PaneLeaf`], so callers must handle their absence instead of assuming
    /// every tab owns a VTE pane.
    pub fn first_leaf(&self) -> Option<&PaneLeaf> {
        match self {
            PaneNode::Leaf(leaf) => Some(leaf),
            PaneNode::Split { first, second, .. } => {
                first.first_leaf().or_else(|| second.first_leaf())
            }
            PaneNode::Stub { .. } | PaneNode::Empty => None,
        }
    }

    /// Check if tree contains a pane with the given ID.
    pub fn contains_pane(&self, pane_id: u32) -> bool {
        match self {
            PaneNode::Leaf(leaf) => leaf.pane_id == pane_id,
            PaneNode::Split { first, second, .. } => {
                first.contains_pane(pane_id) || second.contains_pane(pane_id)
            }
            PaneNode::Stub { pane_id: id } => *id == pane_id,
            PaneNode::Empty => false,
        }
    }

    /// Return the root GTK widget for this pane subtree, when it has one.
    pub fn root_widget(&self) -> Option<gtk::Widget> {
        match self {
            PaneNode::Leaf(leaf) => Some(leaf.container.clone().upcast()),
            PaneNode::Split { widget, .. } => Some(widget.clone().upcast()),
            PaneNode::Stub { .. } | PaneNode::Empty => None,
        }
    }

    /// Return the split widgets on the path from this root to the target pane.
    /// The path is ordered from root-most split to the leaf's direct parent.
    pub fn split_path_to(&self, pane_id: u32) -> Option<Vec<PaneSplitPathEntry>> {
        let mut path = Vec::new();
        if self.collect_split_path(pane_id, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn collect_split_path(&self, pane_id: u32, path: &mut Vec<PaneSplitPathEntry>) -> bool {
        match self {
            PaneNode::Leaf(leaf) => leaf.pane_id == pane_id,
            PaneNode::Split {
                first,
                second,
                widget,
                ..
            } => {
                if first.contains_pane(pane_id) {
                    path.push(PaneSplitPathEntry {
                        widget: widget.clone(),
                        target_child: SplitChild::First,
                    });
                    first.collect_split_path(pane_id, path)
                } else if second.contains_pane(pane_id) {
                    path.push(PaneSplitPathEntry {
                        widget: widget.clone(),
                        target_child: SplitChild::Second,
                    });
                    second.collect_split_path(pane_id, path)
                } else {
                    false
                }
            }
            PaneNode::Stub { pane_id: id } => *id == pane_id,
            PaneNode::Empty => false,
        }
    }

    fn collect_pane_rects(
        &self,
        left: f64,
        top: f64,
        width: f64,
        height: f64,
        rects: &mut Vec<PaneRect>,
    ) {
        match self {
            PaneNode::Leaf(leaf) => rects.push(PaneRect {
                pane_id: leaf.pane_id,
                left,
                top,
                right: left + width,
                bottom: top + height,
            }),
            PaneNode::Split {
                direction,
                first,
                second,
                widget,
            } => {
                let ratio = split_ratio(*direction, widget);
                match direction {
                    SplitDirection::Vertical => {
                        let first_width = width * ratio;
                        first.collect_pane_rects(left, top, first_width, height, rects);
                        second.collect_pane_rects(
                            left + first_width,
                            top,
                            width - first_width,
                            height,
                            rects,
                        );
                    }
                    SplitDirection::Horizontal => {
                        let first_height = height * ratio;
                        first.collect_pane_rects(left, top, width, first_height, rects);
                        second.collect_pane_rects(
                            left,
                            top + first_height,
                            width,
                            height - first_height,
                            rects,
                        );
                    }
                }
            }
            PaneNode::Stub { pane_id } => rects.push(PaneRect {
                pane_id: *pane_id,
                left,
                top,
                right: left + width,
                bottom: top + height,
            }),
            PaneNode::Empty => {}
        }
    }

    /// Find an adjacent pane by spatial direction.
    /// Returns the nearest pane in the requested direction, or None when no
    /// pane exists on that side.
    pub fn find_adjacent_pane(
        &self,
        current_id: u32,
        direction: PaneNavigationDirection,
    ) -> Option<u32> {
        let mut rects = Vec::new();
        self.collect_pane_rects(0.0, 0.0, 1.0, 1.0, &mut rects);
        find_adjacent_rect(&rects, current_id, direction)
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Copy, Debug)]
struct PaneRect {
    pane_id: u32,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl PaneRect {
    fn center_x(self) -> f64 {
        (self.left + self.right) * 0.5
    }

    fn center_y(self) -> f64 {
        (self.top + self.bottom) * 0.5
    }
}

fn split_ratio(direction: SplitDirection, widget: &gtk::Paned) -> f64 {
    let (position, total) = match direction {
        SplitDirection::Vertical => (widget.position(), widget.width()),
        SplitDirection::Horizontal => (widget.position(), widget.height()),
    };

    if total > 0 {
        (position as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn axis_overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

fn axis_gap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    if axis_overlap(a_start, a_end, b_start, b_end) > 0.0 {
        0.0
    } else if a_end <= b_start {
        b_start - a_end
    } else {
        a_start - b_end
    }
}

fn find_adjacent_rect(
    rects: &[PaneRect],
    current_id: u32,
    direction: PaneNavigationDirection,
) -> Option<u32> {
    let current = rects
        .iter()
        .copied()
        .find(|rect| rect.pane_id == current_id)?;
    const EPS: f64 = 0.000_001;

    rects
        .iter()
        .copied()
        .filter(|rect| rect.pane_id != current_id)
        .filter_map(|rect| {
            let (is_candidate, primary_gap, perpendicular_gap, perpendicular_distance) =
                match direction {
                    PaneNavigationDirection::Left => (
                        rect.right <= current.left + EPS,
                        (current.left - rect.right).max(0.0),
                        axis_gap(current.top, current.bottom, rect.top, rect.bottom),
                        (current.center_y() - rect.center_y()).abs(),
                    ),
                    PaneNavigationDirection::Right => (
                        rect.left >= current.right - EPS,
                        (rect.left - current.right).max(0.0),
                        axis_gap(current.top, current.bottom, rect.top, rect.bottom),
                        (current.center_y() - rect.center_y()).abs(),
                    ),
                    PaneNavigationDirection::Up => (
                        rect.bottom <= current.top + EPS,
                        (current.top - rect.bottom).max(0.0),
                        axis_gap(current.left, current.right, rect.left, rect.right),
                        (current.center_x() - rect.center_x()).abs(),
                    ),
                    PaneNavigationDirection::Down => (
                        rect.top >= current.bottom - EPS,
                        (rect.top - current.bottom).max(0.0),
                        axis_gap(current.left, current.right, rect.left, rect.right),
                        (current.center_x() - rect.center_x()).abs(),
                    ),
                };

            is_candidate.then_some((
                rect.pane_id,
                primary_gap,
                perpendicular_gap,
                perpendicular_distance,
            ))
        })
        .min_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.0.cmp(&b.0))
        })
        .map(|candidate| candidate.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a stub leaf for testing navigation.
    fn stub_leaf(pane_id: u32) -> PaneNode {
        PaneNode::Stub { pane_id }
    }

    fn rect(pane_id: u32, left: f64, top: f64, right: f64, bottom: f64) -> PaneRect {
        PaneRect {
            pane_id,
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn test_find_adjacent_pane_single_pane() {
        let tree = stub_leaf(1);

        assert_eq!(
            tree.find_adjacent_pane(1, PaneNavigationDirection::Left),
            None
        );
        assert_eq!(
            tree.find_adjacent_pane(1, PaneNavigationDirection::Right),
            None
        );
        assert_eq!(
            tree.find_adjacent_pane(1, PaneNavigationDirection::Up),
            None
        );
        assert_eq!(
            tree.find_adjacent_pane(1, PaneNavigationDirection::Down),
            None
        );
    }

    #[test]
    fn first_leaf_is_total_for_non_terminal_trees() {
        assert!(PaneNode::Empty.first_leaf().is_none());
        assert!(PaneNode::Stub { pane_id: 7 }.first_leaf().is_none());
        assert!(PaneNode::Empty.root_widget().is_none());
        assert!(PaneNode::Stub { pane_id: 7 }.root_widget().is_none());
    }

    #[test]
    fn test_find_adjacent_rect_uses_spatial_neighbors_in_grid() {
        let rects = vec![
            rect(1, 0.0, 0.0, 0.5, 0.5),
            rect(2, 0.5, 0.0, 1.0, 0.5),
            rect(3, 0.0, 0.5, 0.5, 1.0),
            rect(4, 0.5, 0.5, 1.0, 1.0),
        ];

        assert_eq!(
            find_adjacent_rect(&rects, 1, PaneNavigationDirection::Right),
            Some(2)
        );
        assert_eq!(
            find_adjacent_rect(&rects, 1, PaneNavigationDirection::Down),
            Some(3)
        );
        assert_eq!(
            find_adjacent_rect(&rects, 3, PaneNavigationDirection::Up),
            Some(1)
        );
        assert_eq!(
            find_adjacent_rect(&rects, 4, PaneNavigationDirection::Left),
            Some(3)
        );
    }

    #[test]
    fn test_find_adjacent_rect_does_not_wrap_or_use_dfs_order() {
        let rects = vec![
            rect(1, 0.0, 0.0, 0.5, 0.5),
            rect(2, 0.5, 0.0, 1.0, 0.5),
            rect(3, 0.0, 0.5, 0.5, 1.0),
            rect(4, 0.5, 0.5, 1.0, 1.0),
        ];

        assert_eq!(
            find_adjacent_rect(&rects, 3, PaneNavigationDirection::Left),
            None
        );
        assert_eq!(
            find_adjacent_rect(&rects, 2, PaneNavigationDirection::Up),
            None
        );
        assert_eq!(
            find_adjacent_rect(&rects, 3, PaneNavigationDirection::Right),
            Some(4)
        );
        assert_eq!(
            find_adjacent_rect(&rects, 2, PaneNavigationDirection::Down),
            Some(4)
        );
    }
}
