use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::workspace::WorkspaceAction;

/// Every bindable action in taarof. Using a typed enum means the compiler
/// enforces exhaustiveness — we can't forget to handle a new action — and
/// user typos in keybindings.toml produce a logged warning rather than
/// a silently dead binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)] // Action names mirror the user-facing keybinding/config vocabulary.
pub enum Action {
    NewTab,
    NewTmuxTab,
    PreviousTab,
    SearchToggle,
    JumpAttention,
    CommandPalette,
    ShortcutHelp,
    RegisterProject,
    WorkspaceInspector,
    HistoryView,
    ToggleDock,
    ToggleSidebarCompact,
    LeaderMode,
    QuickAction,
    ToggleBroadcastInput,
    Copy,
    CopyRecentOutput,
    CopyLastMessage,
    Paste,
    ToggleSelectionMode,
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    TogglePaneZoom,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    JumpPreviousPrompt,
    JumpNextPrompt,
    NewWorkspace,
    PreviousWorkspace,
    NextWorkspace,
    PrevWorkspace,
    DiscoverTab,
    SendToPane,
    ClipboardHistory,
    RecentFiles,
    PeekFile,
    // ── Palette- and menu-invoked actions ──
    // These reached GTK through raw `"win.…"` string literals before the typed
    // dispatch landed. They ship unbound (empty `default_trigger`) so adopting
    // them introduces no new default shortcuts, but they are now rebindable in
    // keybindings.toml and visible in the shortcut help like every other action.
    BroadcastTab,
    BroadcastWorkspace,
    BroadcastOff,
    RelayLastMessage,
    SendLastOutputToPane,
}

impl Action {
    pub const ALL: &[Action] = &[
        Action::NewTab,
        Action::NewTmuxTab,
        Action::PreviousTab,
        Action::SearchToggle,
        Action::JumpAttention,
        Action::CommandPalette,
        Action::ShortcutHelp,
        Action::RegisterProject,
        Action::WorkspaceInspector,
        Action::HistoryView,
        Action::ToggleDock,
        Action::ToggleSidebarCompact,
        Action::LeaderMode,
        Action::QuickAction,
        Action::ToggleBroadcastInput,
        Action::Copy,
        Action::CopyRecentOutput,
        Action::CopyLastMessage,
        Action::Paste,
        Action::ToggleSelectionMode,
        Action::SplitVertical,
        Action::SplitHorizontal,
        Action::ClosePane,
        Action::TogglePaneZoom,
        Action::FocusPaneLeft,
        Action::FocusPaneRight,
        Action::FocusPaneUp,
        Action::FocusPaneDown,
        Action::JumpPreviousPrompt,
        Action::JumpNextPrompt,
        Action::NewWorkspace,
        Action::PreviousWorkspace,
        Action::NextWorkspace,
        Action::PrevWorkspace,
        Action::DiscoverTab,
        Action::SendToPane,
        Action::ClipboardHistory,
        Action::RecentFiles,
        Action::PeekFile,
        Action::BroadcastTab,
        Action::BroadcastWorkspace,
        Action::BroadcastOff,
        Action::RelayLastMessage,
        Action::SendLastOutputToPane,
    ];

    /// The GAction name dispatched by GTK (e.g. "win.new-tab").
    pub fn gaction_name(self) -> &'static str {
        match self {
            Action::NewTab => "win.new-tab",
            Action::NewTmuxTab => "win.new-tmux-tab",
            Action::PreviousTab => "win.previous-tab",
            Action::SearchToggle => "win.search-toggle",
            Action::JumpAttention => "win.jump-attention",
            Action::CommandPalette => "win.command-palette",
            Action::ShortcutHelp => "win.shortcut-help",
            Action::RegisterProject => "win.register-project",
            Action::WorkspaceInspector => "win.workspace-inspector",
            Action::HistoryView => "win.history-view",
            Action::ToggleDock => "win.toggle-dock",
            Action::ToggleSidebarCompact => "win.toggle-sidebar-compact",
            Action::LeaderMode => "win.leader-mode",
            Action::QuickAction => "win.quick-action",
            Action::ToggleBroadcastInput => "win.toggle-broadcast-input",
            Action::Copy => "term.copy",
            Action::CopyRecentOutput => "win.copy-recent-output",
            Action::CopyLastMessage => "win.copy-last-message",
            Action::Paste => "win.paste-active-input",
            Action::ToggleSelectionMode => "win.toggle-selection-mode",
            Action::SplitVertical => "win.split-vertical",
            Action::SplitHorizontal => "win.split-horizontal",
            Action::ClosePane => "win.close-pane",
            Action::TogglePaneZoom => "win.toggle-pane-zoom",
            Action::FocusPaneLeft => "win.focus-pane-left",
            Action::FocusPaneRight => "win.focus-pane-right",
            Action::FocusPaneUp => "win.focus-pane-up",
            Action::FocusPaneDown => "win.focus-pane-down",
            Action::JumpPreviousPrompt => "win.jump-previous-prompt",
            Action::JumpNextPrompt => "win.jump-next-prompt",
            Action::NewWorkspace => "win.new-workspace",
            Action::PreviousWorkspace => "win.previous-workspace",
            Action::NextWorkspace => "win.next-workspace",
            Action::PrevWorkspace => "win.prev-workspace",
            Action::DiscoverTab => "win.discover-tab",
            Action::SendToPane => "win.send-to-pane",
            Action::ClipboardHistory => "win.clipboard-history",
            Action::RecentFiles => "win.recent-files",
            Action::PeekFile => "win.peek-file",
            Action::BroadcastTab => "win.broadcast-tab",
            Action::BroadcastWorkspace => "win.broadcast-workspace",
            Action::BroadcastOff => "win.broadcast-off",
            Action::RelayLastMessage => "win.relay-last-message",
            Action::SendLastOutputToPane => "win.send-last-output-to-pane",
        }
    }

    /// Where this action's shortcut must be registered.
    pub fn scope(self) -> ActionScope {
        match self {
            Action::NewTab
            | Action::SearchToggle
            | Action::JumpAttention
            | Action::ShortcutHelp
            | Action::NewWorkspace
            | Action::DiscoverTab => ActionScope::Window,
            // CommandPalette and NewTmuxTab need both: window-level for non-VTE contexts,
            // terminal-level because VTE may consume shortcuts before the
            // ShortcutController sees them.
            Action::NewTmuxTab
            | Action::CommandPalette
            | Action::RegisterProject
            | Action::WorkspaceInspector
            | Action::HistoryView
            | Action::ToggleDock
            | Action::ToggleSidebarCompact
            | Action::LeaderMode
            | Action::QuickAction
            | Action::PreviousTab
            | Action::PreviousWorkspace
            | Action::NextWorkspace
            | Action::PrevWorkspace
            | Action::ToggleBroadcastInput
            // SendToPane and ClipboardHistory open palette pickers; VTE may
            // consume the chord, so they register at both window and terminal
            // scope.
            | Action::SendToPane
            | Action::ClipboardHistory
            | Action::RecentFiles
            | Action::PeekFile
            // Palette/menu actions: registered at both scopes for the same
            // reason as their siblings above — VTE may consume a chord before
            // the window ShortcutController sees it, once a user binds one.
            | Action::BroadcastTab
            | Action::BroadcastWorkspace
            | Action::BroadcastOff
            | Action::RelayLastMessage
            | Action::SendLastOutputToPane => ActionScope::Both,
            Action::Copy
            | Action::CopyRecentOutput
            | Action::CopyLastMessage
            | Action::Paste
            | Action::ToggleSelectionMode
            | Action::SplitVertical
            | Action::SplitHorizontal
            | Action::ClosePane
            | Action::TogglePaneZoom
            | Action::FocusPaneLeft
            | Action::FocusPaneRight
            | Action::FocusPaneUp
            | Action::FocusPaneDown
            | Action::JumpPreviousPrompt
            | Action::JumpNextPrompt => ActionScope::Terminal,
        }
    }

    /// Default GTK trigger string for this action.
    pub fn default_trigger(self) -> &'static str {
        match self {
            Action::NewTab => "<Control>t",
            Action::NewTmuxTab => "<Control><Shift>t",
            Action::PreviousTab => "<Control><Shift>Tab",
            Action::SearchToggle => "<Control><Shift>f",
            Action::JumpAttention => "<Control><Alt>j",
            Action::CommandPalette => "<Control><Shift>p",
            Action::ShortcutHelp => "question",
            Action::RegisterProject => "<Control><Alt>p",
            Action::WorkspaceInspector => "<Control><Shift>i",
            Action::HistoryView => "<Control><Alt>h",
            Action::ToggleDock => "",
            Action::ToggleSidebarCompact => "",
            Action::LeaderMode => "",
            Action::QuickAction => "<Control>F5",
            Action::ToggleBroadcastInput => "<Control><Shift>b",
            Action::Copy => "<Control><Shift>c",
            Action::CopyRecentOutput => "<Control><Shift>o",
            Action::CopyLastMessage => "<Control><Shift>m",
            Action::Paste => "<Control><Shift>v",
            Action::ToggleSelectionMode => "<Control><Shift>s",
            Action::SplitVertical => "<Control><Shift>backslash",
            Action::SplitHorizontal => "<Control><Shift>minus",
            Action::ClosePane => "<Control><Shift>w",
            Action::TogglePaneZoom => "<Control><Shift>z",
            Action::FocusPaneLeft => "<Control><Shift>h",
            Action::FocusPaneRight => "<Control><Shift>l",
            Action::FocusPaneUp => "<Control><Shift>k",
            Action::FocusPaneDown => "<Control><Shift>j",
            Action::JumpPreviousPrompt => "<Control><Shift>Up",
            Action::JumpNextPrompt => "<Control><Shift>Down",
            Action::NewWorkspace => "",
            Action::PreviousWorkspace => "<Control><Alt>Tab",
            Action::NextWorkspace => "<Control>Page_Down",
            Action::PrevWorkspace => "<Control>Page_Up",
            Action::DiscoverTab => "<Control><Shift>d",
            Action::SendToPane => "<Control><Shift>e",
            Action::ClipboardHistory => "<Control><Shift>y",
            Action::RecentFiles => "<Control><Shift>r",
            Action::PeekFile => "<Control><Shift>space",
            // Unbound by default: these were palette-only before typed dispatch,
            // and giving them chords now would silently claim keys from users.
            Action::BroadcastTab => "",
            Action::BroadcastWorkspace => "",
            Action::BroadcastOff => "",
            Action::RelayLastMessage => "",
            Action::SendLastOutputToPane => "",
        }
    }

    /// Human-readable label for the command palette (e.g. "Ctrl+Shift+F").
    pub fn display_label(self, trigger: &str) -> Option<String> {
        if trigger.is_empty() {
            return None;
        }
        // Use gtk::accelerator_parse to canonically decompose, then rebuild
        // a human-readable string. If parsing fails, fall back to the raw trigger.
        if let Some((key, mods)) = gtk::accelerator_parse(trigger) {
            let label = gtk::accelerator_get_label(key, mods);
            if label.is_empty() {
                Some(trigger.to_string())
            } else {
                Some(label.to_string())
            }
        } else {
            Some(trigger.to_string())
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::NewTab => "New Tab",
            Action::NewTmuxTab => "New tmux-backed Tab",
            Action::PreviousTab => "Previous Tab",
            Action::SearchToggle => "Search in Terminal",
            Action::JumpAttention => "Jump to Attention Tab",
            Action::CommandPalette => "Command Palette",
            Action::ShortcutHelp => "Keyboard Shortcuts",
            Action::RegisterProject => "Register Focused Project",
            Action::WorkspaceInspector => "Workspace Inspector",
            Action::HistoryView => "History",
            Action::ToggleDock => "Toggle Right Dock",
            Action::ToggleSidebarCompact => "Toggle Compact Sidebar",
            Action::LeaderMode => "Leader Mode",
            Action::QuickAction => "Quick Actions",
            Action::ToggleBroadcastInput => "Toggle Broadcast Input",
            Action::Copy => "Copy",
            Action::CopyRecentOutput => "Copy Recent Output",
            Action::CopyLastMessage => "Copy Last Agent Message",
            Action::Paste => "Paste",
            Action::ToggleSelectionMode => "Toggle Selection Mode",
            Action::SplitVertical => "Split Vertical",
            Action::SplitHorizontal => "Split Horizontal",
            Action::ClosePane => "Close Pane",
            Action::TogglePaneZoom => "Toggle Pane Zoom",
            Action::FocusPaneLeft => "Focus Pane Left",
            Action::FocusPaneRight => "Focus Pane Right",
            Action::FocusPaneUp => "Focus Pane Up",
            Action::FocusPaneDown => "Focus Pane Down",
            Action::JumpPreviousPrompt => "Jump to Previous Prompt",
            Action::JumpNextPrompt => "Jump to Next Prompt",
            Action::NewWorkspace => "New Workspace",
            Action::PreviousWorkspace => "Previous Workspace",
            Action::NextWorkspace => "Next Workspace",
            Action::PrevWorkspace => "Cycle Previous Workspace",
            Action::DiscoverTab => "Discover Tasks",
            Action::SendToPane => "Send to Pane",
            Action::ClipboardHistory => "Clipboard History",
            Action::RecentFiles => "Recent Files",
            Action::PeekFile => "Peek File",
            Action::BroadcastTab => "Broadcast Input to Tab",
            Action::BroadcastWorkspace => "Broadcast Input to Workspace",
            Action::BroadcastOff => "Broadcast Input Off",
            Action::RelayLastMessage => "Relay Last Agent Message",
            Action::SendLastOutputToPane => "Send Last Output to Pane",
        }
    }

    /// Coarse grouping used to order the keybinding help view into readable
    /// sections. Derived from the action's role, not its GTK scope.
    pub fn category(self) -> ActionCategory {
        match self {
            Action::NewTab
            | Action::NewTmuxTab
            | Action::PreviousTab
            | Action::JumpAttention
            | Action::DiscoverTab
            | Action::SearchToggle => ActionCategory::Tabs,
            Action::SplitVertical
            | Action::SplitHorizontal
            | Action::ClosePane
            | Action::TogglePaneZoom
            | Action::FocusPaneLeft
            | Action::FocusPaneRight
            | Action::FocusPaneUp
            | Action::FocusPaneDown
            | Action::JumpPreviousPrompt
            | Action::JumpNextPrompt => ActionCategory::Panes,
            Action::NewWorkspace
            | Action::PreviousWorkspace
            | Action::NextWorkspace
            | Action::PrevWorkspace
            | Action::WorkspaceInspector
            | Action::RegisterProject => ActionCategory::Workspaces,
            Action::HistoryView | Action::ToggleDock | Action::ToggleSidebarCompact => {
                ActionCategory::General
            }
            Action::Copy
            | Action::CopyRecentOutput
            | Action::CopyLastMessage
            | Action::Paste
            | Action::ToggleSelectionMode
            | Action::ClipboardHistory => ActionCategory::Clipboard,
            Action::CommandPalette
            | Action::ShortcutHelp
            | Action::LeaderMode
            | Action::QuickAction
            | Action::SendToPane
            | Action::RecentFiles
            | Action::PeekFile
            | Action::ToggleBroadcastInput
            | Action::BroadcastTab
            | Action::BroadcastWorkspace
            | Action::BroadcastOff
            | Action::RelayLastMessage
            | Action::SendLastOutputToPane => ActionCategory::General,
        }
    }
}

/// Coarse categories for grouping actions in the keybinding help view. Ordered
/// as they should appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActionCategory {
    General,
    Tabs,
    Panes,
    Workspaces,
    Clipboard,
}

impl ActionCategory {
    /// Every category in display order.
    pub const ALL: &[ActionCategory] = &[
        ActionCategory::General,
        ActionCategory::Tabs,
        ActionCategory::Panes,
        ActionCategory::Workspaces,
        ActionCategory::Clipboard,
    ];

    /// Human-readable section heading.
    pub fn title(self) -> &'static str {
        match self {
            ActionCategory::General => "General",
            ActionCategory::Tabs => "Tabs",
            ActionCategory::Panes => "Panes",
            ActionCategory::Workspaces => "Workspaces",
            ActionCategory::Clipboard => "Clipboard & Selection",
        }
    }
}

/// One row of the keybinding help view: an action, its display name, its
/// currently-effective trigger label, and whether that trigger differs from the
/// compiled-in default (i.e. was customized via keybindings.toml).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveBinding {
    pub action: Action,
    /// User-facing action name (e.g. "New Tab").
    pub display_name: &'static str,
    /// Human-readable trigger label (e.g. "Ctrl+Shift+P"), or `None` if unbound.
    pub trigger: Option<String>,
    /// True when the effective trigger differs from the action's default.
    pub customized: bool,
    /// Category used for grouping/ordering in the help view.
    pub category: ActionCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionScope {
    Window,
    Terminal,
    Both,
}

/// A typed target selected by a follow-up chord.
///
/// Bindable application actions and discovered workspace tasks have different
/// execution paths. Keeping that distinction in the value prevents a chord
/// from becoming a synthesized GTK action name at the UI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordTarget {
    Bindable(Action),
    Workspace(WorkspaceAction),
}

/// Runtime ledger for the bindable window actions installed by `lib.rs`.
///
/// GTK action names are stringly typed at the final boundary. Recording every
/// registration through this ledger keeps that boundary tied to [`Action`]: a
/// missing, duplicated, or renamed handler aborts startup instead of leaving a
/// shortcut that GTK silently ignores.
#[derive(Default)]
pub struct WindowActionLedger {
    registrations: Vec<(Action, String)>,
}

impl WindowActionLedger {
    /// Record one bindable window handler immediately before it is added.
    pub fn register(&mut self, action: Action, name: &str) {
        assert!(
            action.gaction_name().starts_with("win."),
            "only win.* actions can be registered as window handlers"
        );
        self.registrations.push((action, name.to_string()));
    }

    /// Verify that the installed handlers and the bindable action vocabulary
    /// are a one-to-one mapping.
    pub fn validate(&self) -> Result<(), String> {
        let registrations: Vec<(Action, &str)> = self
            .registrations
            .iter()
            .map(|(action, name)| (*action, name.as_str()))
            .collect();
        validate_window_action_registrations(&registrations)
    }

    /// Names of handlers that were added through the bindable registration seam.
    pub fn handler_names(&self) -> impl Iterator<Item = &str> {
        self.registrations.iter().map(|(_, name)| name.as_str())
    }
}

/// Validate a declarative window-action registration set.
///
/// Every `win.*` [`Action`] is bindable and therefore must have one handler;
/// terminal-only `term.copy` is intentionally absent because VTE consumes it
/// before a window action group can see it. Callers can only construct a
/// [`WindowActionLedger`] with an [`Action`], which prevents an orphan window
/// handler from bypassing the keybinding and shortcut-help vocabulary.
pub fn validate_window_action_registrations(
    registrations: &[(Action, &str)],
) -> Result<(), String> {
    let expected: Vec<(Action, &str)> = Action::ALL
        .iter()
        .copied()
        .filter_map(|action| {
            action
                .gaction_name()
                .strip_prefix("win.")
                .map(|name| (action, name))
        })
        .collect();

    let mut errors = Vec::new();
    if registrations.is_empty() {
        errors.push("no bindable window actions were registered".to_string());
    }
    for (action, expected_name) in &expected {
        let names: Vec<&str> = registrations
            .iter()
            .filter_map(|(registered_action, registered_name)| {
                (*registered_action == *action).then_some(*registered_name)
            })
            .collect();
        match names.as_slice() {
            [] => errors.push(format!("missing handler for win.{expected_name}")),
            [registered_name] if *registered_name != *expected_name => errors.push(format!(
                "handler for {action} was renamed to win.{registered_name}; expected win.{expected_name}"
            )),
            [_] => {}
            _ => errors.push(format!("duplicate handlers for win.{expected_name}")),
        }
    }

    for (action, name) in registrations {
        if !expected
            .iter()
            .any(|(expected_action, _)| action == expected_action)
        {
            errors.push(format!("unbound Action {action} registered as win.{name}"));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::NewTab => "new-tab",
            Action::NewTmuxTab => "new-tmux-tab",
            Action::PreviousTab => "previous-tab",
            Action::SearchToggle => "search-toggle",
            Action::JumpAttention => "jump-attention",
            Action::CommandPalette => "command-palette",
            Action::ShortcutHelp => "shortcut-help",
            Action::RegisterProject => "register-project",
            Action::WorkspaceInspector => "workspace-inspector",
            Action::HistoryView => "history-view",
            Action::ToggleDock => "toggle-dock",
            Action::ToggleSidebarCompact => "toggle-sidebar-compact",
            Action::LeaderMode => "leader-mode",
            Action::QuickAction => "quick-action",
            Action::ToggleBroadcastInput => "toggle-broadcast-input",
            Action::Copy => "copy",
            Action::CopyRecentOutput => "copy-recent-output",
            Action::CopyLastMessage => "copy-last-message",
            Action::Paste => "paste",
            Action::ToggleSelectionMode => "toggle-selection-mode",
            Action::SplitVertical => "split-vertical",
            Action::SplitHorizontal => "split-horizontal",
            Action::ClosePane => "close-pane",
            Action::TogglePaneZoom => "toggle-pane-zoom",
            Action::FocusPaneLeft => "focus-pane-left",
            Action::FocusPaneRight => "focus-pane-right",
            Action::FocusPaneUp => "focus-pane-up",
            Action::FocusPaneDown => "focus-pane-down",
            Action::JumpPreviousPrompt => "jump-previous-prompt",
            Action::JumpNextPrompt => "jump-next-prompt",
            Action::NewWorkspace => "new-workspace",
            Action::PreviousWorkspace => "previous-workspace",
            Action::NextWorkspace => "next-workspace",
            Action::PrevWorkspace => "prev-workspace",
            Action::DiscoverTab => "discover-tab",
            Action::SendToPane => "send-to-pane",
            Action::ClipboardHistory => "clipboard-history",
            Action::RecentFiles => "recent-files",
            Action::PeekFile => "peek-file",
            Action::BroadcastTab => "broadcast-tab",
            Action::BroadcastWorkspace => "broadcast-workspace",
            Action::BroadcastOff => "broadcast-off",
            Action::RelayLastMessage => "relay-last-message",
            Action::SendLastOutputToPane => "send-last-output-to-pane",
        })
    }
}

impl FromStr for Action {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "new-tab" => Ok(Action::NewTab),
            "new-tmux-tab" => Ok(Action::NewTmuxTab),
            "previous-tab" => Ok(Action::PreviousTab),
            "search-toggle" => Ok(Action::SearchToggle),
            "jump-attention" => Ok(Action::JumpAttention),
            "command-palette" => Ok(Action::CommandPalette),
            "shortcut-help" => Ok(Action::ShortcutHelp),
            "register-project" => Ok(Action::RegisterProject),
            "workspace-inspector" => Ok(Action::WorkspaceInspector),
            "history-view" => Ok(Action::HistoryView),
            "toggle-dock" => Ok(Action::ToggleDock),
            "toggle-sidebar-compact" => Ok(Action::ToggleSidebarCompact),
            "leader-mode" => Ok(Action::LeaderMode),
            "quick-action" => Ok(Action::QuickAction),
            "toggle-broadcast-input" => Ok(Action::ToggleBroadcastInput),
            "copy" => Ok(Action::Copy),
            "copy-recent-output" => Ok(Action::CopyRecentOutput),
            "copy-last-message" => Ok(Action::CopyLastMessage),
            "paste" => Ok(Action::Paste),
            "toggle-selection-mode" => Ok(Action::ToggleSelectionMode),
            "split-vertical" => Ok(Action::SplitVertical),
            "split-horizontal" => Ok(Action::SplitHorizontal),
            "close-pane" => Ok(Action::ClosePane),
            "toggle-pane-zoom" => Ok(Action::TogglePaneZoom),
            "focus-pane-left" => Ok(Action::FocusPaneLeft),
            "focus-pane-right" => Ok(Action::FocusPaneRight),
            "focus-pane-up" => Ok(Action::FocusPaneUp),
            "focus-pane-down" => Ok(Action::FocusPaneDown),
            "jump-previous-prompt" => Ok(Action::JumpPreviousPrompt),
            "jump-next-prompt" => Ok(Action::JumpNextPrompt),
            "new-workspace" => Ok(Action::NewWorkspace),
            "previous-workspace" => Ok(Action::PreviousWorkspace),
            "next-workspace" => Ok(Action::NextWorkspace),
            "prev-workspace" => Ok(Action::PrevWorkspace),
            "discover-tab" => Ok(Action::DiscoverTab),
            "send-to-pane" => Ok(Action::SendToPane),
            "clipboard-history" => Ok(Action::ClipboardHistory),
            "recent-files" => Ok(Action::RecentFiles),
            "peek-file" => Ok(Action::PeekFile),
            "broadcast-tab" => Ok(Action::BroadcastTab),
            "broadcast-workspace" => Ok(Action::BroadcastWorkspace),
            "broadcast-off" => Ok(Action::BroadcastOff),
            "relay-last-message" => Ok(Action::RelayLastMessage),
            "send-last-output-to-pane" => Ok(Action::SendLastOutputToPane),
            other => Err(format!("unknown action: {other}")),
        }
    }
}

/// Resolved keybinding configuration: every action has a validated trigger string.
#[derive(Debug, Clone)]
pub struct KeybindingConfig {
    /// Action → GTK trigger string (e.g. "<Control>t"). Empty string = unbound.
    shortcuts: BTreeMap<Action, String>,
    /// Follow-up key → typed workspace task for the quick-action overlay.
    pub quick_action_map: BTreeMap<char, WorkspaceAction>,
    /// Follow-up key → built-in taarof action for leader mode.
    pub leader_map: BTreeMap<char, Action>,
}

impl KeybindingConfig {
    /// Load from ~/.config/taarof/keybindings.toml, validating each entry.
    /// Invalid triggers fall back to the action's default. Missing file
    /// is created with defaults.
    pub fn load() -> Self {
        let path = config_path();
        let file_contents = match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(_) => {
                // Generate default config file
                let cfg = Self::defaults();
                if let Err(e) = cfg.write_default_file(&path) {
                    eprintln!("taarof: could not write default keybindings.toml: {e}");
                }
                None
            }
        };

        let Some(contents) = file_contents else {
            return Self::defaults();
        };

        let raw: RawKeybindingFile = match toml::from_str(&contents) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("taarof: failed to parse keybindings.toml: {e}");
                eprintln!("taarof: falling back to default keybindings");
                return Self::defaults();
            }
        };

        let shortcuts = load_shortcuts(&raw.shortcuts);
        let (quick_action_map, leader_map) = load_chords(raw.chords.as_ref());

        KeybindingConfig {
            shortcuts,
            quick_action_map,
            leader_map,
        }
    }

    fn defaults() -> Self {
        let mut shortcuts = BTreeMap::new();
        for &action in Action::ALL {
            shortcuts.insert(action, action.default_trigger().to_string());
        }
        KeybindingConfig {
            shortcuts,
            quick_action_map: default_quick_action_map(),
            leader_map: default_leader_map(),
        }
    }

    /// Get the trigger string for an action. Empty string means unbound.
    pub fn trigger(&self, action: Action) -> &str {
        self.shortcuts
            .get(&action)
            .map(String::as_str)
            .unwrap_or(action.default_trigger())
    }

    /// Get a human-readable label for the palette (e.g. "Ctrl+Shift+F").
    /// Returns None if the action is unbound.
    pub fn display_label(&self, action: Action) -> Option<String> {
        let trigger = self.trigger(action);
        action.display_label(trigger)
    }

    /// Resolve every action to its currently-effective binding, ready for the
    /// keybinding help view. Reflects `keybindings.toml` overrides (the
    /// `customized` flag marks any action whose trigger differs from its
    /// compiled-in default). Rows are ordered by [`ActionCategory`], then by
    /// display name within a category, so the help view is stable and grouped.
    ///
    /// Generated from the live config at call time — never a static snapshot.
    pub fn effective_bindings(&self) -> Vec<EffectiveBinding> {
        let mut rows: Vec<EffectiveBinding> = Action::ALL
            .iter()
            .map(|&action| {
                let trigger_str = self.trigger(action);
                EffectiveBinding {
                    action,
                    display_name: action.label(),
                    trigger: humanize_trigger(trigger_str),
                    customized: trigger_str != action.default_trigger(),
                    category: action.category(),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then_with(|| a.display_name.cmp(b.display_name))
        });
        rows
    }

    /// Concise next-action hints for the empty-workspace surface. Each line
    /// pairs a starter action with its currently-effective trigger, so the
    /// hints never drift from `keybindings.toml`. Actions that are unbound are
    /// still listed (with the trigger portion omitted) so the user learns the
    /// command exists — they can reach it from the command palette.
    pub fn hint_lines(&self) -> Vec<String> {
        const HINT_ACTIONS: &[(Action, &str)] = &[
            (Action::NewTab, "New tab"),
            (Action::NewTmuxTab, "New tmux-backed tab"),
            (Action::SplitVertical, "Split the pane"),
            (Action::CommandPalette, "Command palette"),
            (Action::NextWorkspace, "Next workspace"),
        ];
        HINT_ACTIONS
            .iter()
            .map(
                |&(action, text)| match humanize_trigger(self.trigger(action)) {
                    Some(label) => format!("{text}  ·  {label}"),
                    None => text.to_string(),
                },
            )
            .collect()
    }

    /// Iterate over all actions that should be registered on the window ShortcutController.
    pub fn window_shortcuts(&self) -> Vec<(Action, &str)> {
        self.shortcuts
            .iter()
            .filter(|(action, trigger)| {
                !trigger.is_empty()
                    && matches!(action.scope(), ActionScope::Window | ActionScope::Both)
            })
            .map(|(&action, trigger)| (action, trigger.as_str()))
            .collect()
    }

    /// Iterate over all actions that need terminal-level key interception.
    /// Returns parsed (key, modifiers) tuples for EventControllerKey matching.
    pub fn terminal_shortcuts(&self) -> Vec<TerminalBinding> {
        self.shortcuts_for_scopes(|scope| {
            matches!(scope, ActionScope::Terminal | ActionScope::Both)
        })
    }

    pub fn all_shortcuts(&self) -> Vec<TerminalBinding> {
        self.shortcuts_for_scopes(|_| true)
    }

    fn shortcuts_for_scopes(&self, include: impl Fn(ActionScope) -> bool) -> Vec<TerminalBinding> {
        let mut bindings = Vec::new();
        for (&action, trigger) in &self.shortcuts {
            if trigger.is_empty() {
                continue;
            }
            if !include(action.scope()) {
                continue;
            }
            let Some((key, mods)) = gtk::accelerator_parse(trigger) else {
                continue;
            };
            bindings.push(TerminalBinding { action, key, mods });
        }
        bindings
    }

    fn write_default_file(&self, path: &PathBuf) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut raw = RawKeybindingFile::default();
        for &action in Action::ALL {
            raw.shortcuts
                .insert(action.to_string(), action.default_trigger().to_string());
        }
        let mut qa = BTreeMap::new();
        for (ch, action) in &self.quick_action_map {
            qa.insert(ch.to_string(), action.task_name().to_string());
        }
        let mut leader = BTreeMap::new();
        for (ch, action) in &self.leader_map {
            leader.insert(ch.to_string(), action.to_string());
        }
        raw.chords = Some(RawChords {
            quick_action: Some(qa),
            leader: (!leader.is_empty()).then_some(leader),
        });

        let content = toml::to_string_pretty(&raw).map_err(std::io::Error::other)?;

        std::fs::write(
            path,
            format!(
                "# taarof keybinding configuration\n# Use GTK trigger syntax: <Control>, <Shift>, <Alt>\n# Set to \"\" or \"none\" to disable a shortcut\n# Optional leader mode example:\n# [shortcuts]\n# leader-mode = \"<Control>b\"\n#\n# [chords.leader]\n# c = \"new-tab\"\n# % = \"split-vertical\"\n# \" = \"split-horizontal\"\n\n{content}"
            ),
        )
    }
}

/// A parsed terminal-level keybinding for EventControllerKey matching.
#[derive(Debug, Clone)]
pub struct TerminalBinding {
    pub action: Action,
    pub key: gdk::Key,
    pub mods: gdk::ModifierType,
}

impl TerminalBinding {
    /// Check if a key event matches this binding. When Shift is part of the
    /// binding's modifiers, the compositor may report either the base keyval
    /// (e.g. `backslash`) or the shifted keyval (e.g. `bar`). We handle this
    /// in two ways:
    /// 1. Compare uppercase forms (works for letter keys: a → A)
    /// 2. Resolve the unshifted base keyval via hardware keycode (works for
    ///    symbol keys: backslash → bar, minus → underscore)
    pub fn matches(
        &self,
        pressed_key: gdk::Key,
        pressed_mods: gdk::ModifierType,
        keycode: u32,
    ) -> bool {
        if !pressed_mods.contains(self.mods) {
            return false;
        }
        if pressed_key == self.key {
            return true;
        }
        if self.mods.contains(gdk::ModifierType::SHIFT_MASK) {
            // Letter keys: to_upper() maps both sides to the same value
            if pressed_key.to_upper() == self.key.to_upper() {
                return true;
            }
            // Symbol keys (backslash→bar, minus→underscore): resolve the
            // unshifted base keyval from the hardware keycode.
            use gdk::prelude::DisplayExtManual;
            if let Some(display) = gdk::Display::default() {
                if let Some((base_key, _, _, _)) =
                    display.translate_key(keycode, gdk::ModifierType::empty(), 0)
                {
                    return base_key == self.key;
                }
            }
        }
        false
    }
}

thread_local! {
    static TERMINAL_BINDINGS: RefCell<Vec<TerminalBinding>> = const { RefCell::new(Vec::new()) };
    static SHORTCUT_BINDINGS: RefCell<Vec<TerminalBinding>> = const { RefCell::new(Vec::new()) };
    static INSTALLED_CONFIG: RefCell<Option<KeybindingConfig>> = const { RefCell::new(None) };
}

/// Install keybinding config for use by all VTE terminals and the palette.
/// Called once at startup from build_ui.
pub fn install(config: &KeybindingConfig) {
    let bindings = config.terminal_shortcuts();
    let shortcut_bindings = config.all_shortcuts();
    TERMINAL_BINDINGS.with(|cell| {
        *cell.borrow_mut() = bindings;
    });
    SHORTCUT_BINDINGS.with(|cell| {
        *cell.borrow_mut() = shortcut_bindings;
    });
    INSTALLED_CONFIG.with(|cell| {
        *cell.borrow_mut() = Some(config.clone());
    });
}

/// Read the installed terminal bindings. Used by terminal.rs to set up
/// per-terminal EventControllerKey handlers.
pub fn get_terminal_bindings() -> Vec<TerminalBinding> {
    TERMINAL_BINDINGS.with(|cell| cell.borrow().clone())
}

/// Activate an action's GAction through `widget`'s action group.
///
/// This is the single typed path from a UI surface — palette, sidebar, terminal
/// context menu, inspector — to a GTK action. Taking [`Action`] instead of a
/// `&str` is the whole point: GTK resolves action names at runtime and silently
/// does nothing when one is missing, so a raw `"win.…"` literal turns a rename
/// or a typo into a dead button that still compiles. Routing through
/// [`Action::gaction_name`] makes that a build failure instead.
pub fn activate(widget: &impl gtk::prelude::IsA<gtk::Widget>, action: Action) {
    let _ = gtk::prelude::WidgetExt::activate_action(widget, action.gaction_name(), None);
}

pub fn matches_any_shortcut(
    pressed_key: gdk::Key,
    pressed_mods: gdk::ModifierType,
    keycode: u32,
) -> bool {
    SHORTCUT_BINDINGS.with(|cell| {
        cell.borrow()
            .iter()
            .any(|binding| binding.matches(pressed_key, pressed_mods, keycode))
    })
}

/// Get the display label for an action from the installed config.
/// Returns None if the action is unbound or config hasn't been installed.
pub fn label_for(action: Action) -> Option<String> {
    INSTALLED_CONFIG.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|cfg| cfg.display_label(action))
    })
}

/// Resolve every action's currently-effective binding from the installed
/// config, for the keybinding help view. Falls back to compiled-in defaults
/// when `install` hasn't run yet (e.g. very early startup), so the help view
/// always has content.
pub fn installed_effective_bindings() -> Vec<EffectiveBinding> {
    INSTALLED_CONFIG.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|cfg| cfg.effective_bindings())
            .unwrap_or_else(|| KeybindingConfig::defaults().effective_bindings())
    })
}

/// Empty-workspace next-action hint lines from the installed config, falling
/// back to defaults when `install` hasn't run yet.
pub fn installed_hint_lines() -> Vec<String> {
    INSTALLED_CONFIG.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|cfg| cfg.hint_lines())
            .unwrap_or_else(|| KeybindingConfig::defaults().hint_lines())
    })
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("taarof/keybindings.toml")
}

/// Resolved path to `keybindings.toml` (`~/.config/taarof/keybindings.toml`).
pub fn keybindings_config_path() -> PathBuf {
    config_path()
}

/// Validate the keybindings file at `path` with the same rules the live loader
/// uses (unknown actions, invalid triggers, malformed chords/leader entries),
/// but reporting each problem as a [`Finding`] instead of a stderr warning.
///
/// A missing file is not an error (defaults are used): returns an empty Vec.
/// Runs without any GTK initialization.
pub fn validate_keybindings_file(path: &std::path::Path) -> Vec<crate::config::Finding> {
    use crate::config::Finding;
    const FILE: &str = "keybindings.toml";

    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            return vec![Finding::error(
                FILE,
                format!("could not read {}: {err}", path.display()),
            )];
        }
    };

    let raw: RawKeybindingFile = match toml::from_str(&contents) {
        Ok(raw) => raw,
        Err(err) => {
            return vec![Finding::error(FILE, format!("parse error: {err}"))];
        }
    };

    let mut findings = Vec::new();

    // [shortcuts]: keys must be known actions, values must be valid triggers.
    for (action_name, trigger) in &raw.shortcuts {
        if Action::from_str(action_name).is_err() {
            findings.push(Finding::error(
                FILE,
                format!("unknown action \"{action_name}\" in [shortcuts]"),
            ));
            continue;
        }
        // Empty / "none" / "disabled" explicitly unbinds an action — allowed.
        if trigger.is_empty() || trigger == "none" || trigger == "disabled" {
            continue;
        }
        if !trigger_is_structurally_valid(trigger) {
            findings.push(Finding::error(
                FILE,
                format!("invalid trigger \"{trigger}\" for action \"{action_name}\""),
            ));
        }
    }

    if let Some(chords) = raw.chords.as_ref() {
        if let Some(qa) = chords.quick_action.as_ref() {
            for key_str in qa.keys() {
                if parse_followup_key(key_str).is_none() {
                    findings.push(Finding::error(
                        FILE,
                        format!(
                            "[chords.quick-action] key \"{key_str}\" must be a single character"
                        ),
                    ));
                }
            }
        }
        if let Some(leader) = chords.leader.as_ref() {
            for (key_str, action_name) in leader {
                if parse_followup_key(key_str).is_none() {
                    findings.push(Finding::error(
                        FILE,
                        format!("[chords.leader] key \"{key_str}\" must be a single character"),
                    ));
                }
                match Action::from_str(action_name) {
                    Ok(action) if !leader_target_allowed(action) => findings.push(Finding::error(
                        FILE,
                        format!("[chords.leader] action \"{action_name}\" is not allowed"),
                    )),
                    Ok(_) => {}
                    Err(_) => findings.push(Finding::error(
                        FILE,
                        format!("[chords.leader] unknown action \"{action_name}\""),
                    )),
                }
            }
        }
    }

    findings
}

pub fn default_quick_action_map() -> BTreeMap<char, WorkspaceAction> {
    BTreeMap::from([
        ('d', WorkspaceAction::Dev),
        ('t', WorkspaceAction::Test),
        ('b', WorkspaceAction::Build),
        ('l', WorkspaceAction::Lint),
        ('s', WorkspaceAction::Setup),
    ])
}

#[cfg(test)]
pub fn default_chord_map() -> BTreeMap<char, WorkspaceAction> {
    default_quick_action_map()
}

pub fn default_leader_map() -> BTreeMap<char, Action> {
    BTreeMap::new()
}

pub fn followup_key_from_keyval(key: gdk::Key) -> Option<char> {
    key.to_unicode().map(normalize_followup_char)
}

fn parse_followup_key(key_str: &str) -> Option<char> {
    let mut chars = key_str.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(normalize_followup_char(ch))
}

fn normalize_followup_char(ch: char) -> char {
    if ch.is_ascii_alphabetic() {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

fn leader_target_allowed(action: Action) -> bool {
    !matches!(action, Action::LeaderMode | Action::QuickAction)
}

/// Validate a GTK trigger string by attempting to parse it.
///
/// Used on the live-load path (post GTK-init).
fn is_valid_trigger(trigger: &str) -> bool {
    gtk::accelerator_parse(trigger).is_some()
}

/// Recognized modifier tokens in a GTK accelerator string (case-insensitive).
/// Mirrors the modifiers `gtk_accelerator_parse` understands.
const KNOWN_MODIFIERS: &[&str] = &[
    "shift", "control", "ctrl", "primary", "alt", "meta", "super", "hyper", "release",
];

/// Convert a GTK accelerator/trigger string into a human-readable label
/// (`<Control><Shift>p` → `Ctrl+Shift+P`) **without** requiring GTK to be
/// initialized. This mirrors what `gtk::accelerator_get_label` produces closely
/// enough for the help view and, crucially, lets the keybinding resolver run in
/// headless unit tests. Returns `None` for an empty/unbound trigger.
pub(crate) fn humanize_trigger(trigger: &str) -> Option<String> {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return None;
    }
    let mut rest = trigger;
    let mut parts: Vec<String> = Vec::new();

    // Consume leading <Modifier> tokens, normalizing their display names.
    while let Some(stripped) = rest.strip_prefix('<') {
        let Some(end) = stripped.find('>') else { break };
        let name = &stripped[..end];
        let pretty = match name.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl",
            "shift" => "Shift",
            "alt" | "meta" => "Alt",
            "super" | "hyper" => "Super",
            other => {
                // Unknown modifier: title-case it rather than dropping it.
                return Some(format!(
                    "{}{}",
                    other
                        .chars()
                        .next()
                        .map(|c| c.to_ascii_uppercase())
                        .unwrap_or(' '),
                    &other[other.len().min(1)..]
                ));
            }
        };
        parts.push(pretty.to_string());
        rest = &stripped[end + 1..];
    }

    let key = rest.trim();
    if key.is_empty() {
        // Modifiers with no key — surface what we have.
        return (!parts.is_empty()).then(|| parts.join("+"));
    }
    parts.push(humanize_key_name(key));
    Some(parts.join("+"))
}

/// Human-readable form of a single key token: single chars are upper-cased,
/// GDK key names are given friendly spellings where it matters.
fn humanize_key_name(key: &str) -> String {
    match key {
        "backslash" => return "\\".to_string(),
        "minus" => return "-".to_string(),
        "Page_Up" => return "Page Up".to_string(),
        "Page_Down" => return "Page Down".to_string(),
        _ => {}
    }
    let mut chars = key.chars();
    let first = chars.next();
    match (first, chars.next()) {
        // Single character: upper-case it (t → T).
        (Some(c), None) => c.to_ascii_uppercase().to_string(),
        // Multi-character name (Tab, F5, Escape): keep as written.
        _ => key.to_string(),
    }
}

/// Structurally validate a GTK accelerator/trigger string *without* requiring
/// GTK to be initialized, so the headless `config-validate` command can run in
/// a plain shell.
///
/// Accepts a run of `<Modifier>` tokens (each must be a known modifier) followed
/// by a single non-empty key token (a single printable char or a key name like
/// `Tab`, `F5`, `Page_Down`, `backslash`). This is intentionally at least as
/// strict as the GTK parser for the cases the config validator cares about
/// (unknown modifiers, empty/garbage keys); a structurally-valid string here
/// would also parse under GTK at runtime.
fn trigger_is_structurally_valid(trigger: &str) -> bool {
    let mut rest = trigger.trim();
    if rest.is_empty() {
        return false;
    }

    // Consume leading <Modifier> tokens.
    while let Some(stripped) = rest.strip_prefix('<') {
        let Some(end) = stripped.find('>') else {
            return false; // unterminated "<…"
        };
        let name = &stripped[..end];
        if !KNOWN_MODIFIERS.iter().any(|m| m.eq_ignore_ascii_case(name)) {
            return false; // unknown modifier
        }
        rest = &stripped[end + 1..];
    }

    // What remains must be a single, brace-free key token.
    if rest.is_empty() || rest.contains('<') || rest.contains('>') {
        return false;
    }
    key_name_is_plausible(rest)
}

/// Whether `key` is a plausible GDK key name: a single printable character, or a
/// name made of ASCII alphanumerics and underscores (e.g. `Tab`, `Page_Down`,
/// `F5`), or one of the punctuation key names GTK accepts (`backslash`,
/// `minus`, etc. — those are already alphabetic and covered).
fn key_name_is_plausible(key: &str) -> bool {
    let mut chars = key.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if chars.next().is_none() {
        // Single-character key: any non-whitespace printable char is fine.
        return !first.is_whitespace();
    }
    // Multi-character key name: ASCII alphanumerics and underscores only.
    key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Raw TOML file structure (string-keyed for serde).
#[derive(Debug, Default, Deserialize, Serialize)]
struct RawKeybindingFile {
    #[serde(default)]
    shortcuts: BTreeMap<String, String>,
    #[serde(default)]
    chords: Option<RawChords>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RawChords {
    #[serde(default, rename = "quick-action")]
    quick_action: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "leader")]
    leader: Option<BTreeMap<String, String>>,
}

/// Build the shortcuts map from the raw TOML shortcuts table.
/// Starts with all action defaults, then overrides with the user's values,
/// validating each entry and logging a warning for invalid triggers.
fn load_shortcuts(raw_shortcuts: &BTreeMap<String, String>) -> BTreeMap<Action, String> {
    let mut shortcuts = BTreeMap::new();
    for &action in Action::ALL {
        shortcuts.insert(action, action.default_trigger().to_string());
    }
    for (key, value) in raw_shortcuts {
        let action = match Action::from_str(key) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("taarof: keybindings.toml: unknown action \"{key}\", ignoring");
                continue;
            }
        };
        if value.is_empty() || value == "none" || value == "disabled" {
            shortcuts.insert(action, String::new());
            continue;
        }
        if is_valid_trigger(value) {
            shortcuts.insert(action, value.clone());
        } else {
            eprintln!(
                "taarof: keybindings.toml: invalid trigger \"{}\" for action \"{}\", using default",
                value, key
            );
        }
    }
    shortcuts
}

/// Build the quick-action and leader maps from the raw chords section.
/// Returns `(quick_action_map, leader_map)`.
fn load_chords(
    raw_chords: Option<&RawChords>,
) -> (BTreeMap<char, WorkspaceAction>, BTreeMap<char, Action>) {
    let mut quick_action_map = default_quick_action_map();
    let mut leader_map = default_leader_map();
    let Some(chords) = raw_chords else {
        return (quick_action_map, leader_map);
    };
    if let Some(ref qa) = chords.quick_action {
        quick_action_map.clear();
        for (key_str, task_name) in qa {
            match parse_followup_key(key_str) {
                Some(ch) => {
                    let Some(action) = WorkspaceAction::from_task_name(task_name) else {
                        eprintln!(
                            "taarof: keybindings.toml: quick-action task \"{task_name}\" is not supported"
                        );
                        continue;
                    };
                    quick_action_map.insert(ch, action);
                }
                None => eprintln!(
                    "taarof: keybindings.toml: chord key must be a single character, got \"{key_str}\""
                ),
            }
        }
    }
    if let Some(ref leader) = chords.leader {
        load_leader_map(&mut leader_map, leader);
    }
    (quick_action_map, leader_map)
}

/// Populate `leader_map` from the raw leader section, logging warnings for
/// invalid keys or disallowed actions.
fn load_leader_map(leader_map: &mut BTreeMap<char, Action>, raw: &BTreeMap<String, String>) {
    leader_map.clear();
    for (key_str, action_name) in raw {
        let Some(ch) = parse_followup_key(key_str) else {
            eprintln!(
                "taarof: keybindings.toml: leader key must be a single character, got \"{key_str}\""
            );
            continue;
        };
        let action = match Action::from_str(action_name) {
            Ok(action) => action,
            Err(_) => {
                eprintln!(
                    "taarof: keybindings.toml: unknown leader action \"{action_name}\", ignoring"
                );
                continue;
            }
        };
        if !leader_target_allowed(action) {
            eprintln!("taarof: keybindings.toml: leader action \"{action_name}\" is not allowed");
            continue;
        }
        leader_map.insert(ch, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn direct_activate_action_calls(source: &str) -> Vec<&str> {
        let mut calls = Vec::new();
        let mut remaining = source;
        while let Some(start) = remaining.find("activate_action(") {
            let call_start = &remaining[start..];
            let mut depth = 0_u32;
            let mut end = None;
            for (index, character) in call_start["activate_action".len()..].char_indices() {
                match character {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some("activate_action".len() + index + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                break;
            };
            calls.push(&call_start[..end]);
            remaining = &call_start[end..];
        }
        calls
    }

    fn compact(source: &str) -> String {
        source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn is_documented_activation(path: &Path, call: &str) -> bool {
        let call = compact(call);
        match path.to_string_lossy().replace('\\', "/").as_str() {
            path if path.ends_with("keybindings.rs") => {
                call == "activate_action(widget,action.gaction_name(),None)"
            }
            path if path.ends_with("history_view/ui.rs") => {
                call == "activate_action(&application,\"focus-pane\",Some(&value.to_variant()),)"
            }
            _ => false,
        }
    }

    fn production_source(source: &str) -> &str {
        source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .unwrap_or(source)
    }

    fn untyped_gtk_activations(path: &Path, source: &str) -> Vec<String> {
        direct_activate_action_calls(production_source(source))
            .into_iter()
            .filter(|call| !is_documented_activation(path, call))
            .map(str::to_string)
            .collect()
    }

    fn rust_sources(dir: &Path, sources: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let entry = entry.expect("read source entry");
            let path = entry.path();
            if path.is_dir() {
                rust_sources(&path, sources);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }

    fn release_shortcut_contract() -> &'static [(Action, &'static str)] {
        &[
            (Action::NewTab, "<Control>t"),
            (Action::NewTmuxTab, "<Control><Shift>t"),
            (Action::PreviousTab, "<Control><Shift>Tab"),
            (Action::SearchToggle, "<Control><Shift>f"),
            (Action::JumpAttention, "<Control><Alt>j"),
            (Action::CommandPalette, "<Control><Shift>p"),
            (Action::ShortcutHelp, "question"),
            (Action::RegisterProject, "<Control><Alt>p"),
            (Action::WorkspaceInspector, "<Control><Shift>i"),
            (Action::HistoryView, "<Control><Alt>h"),
            (Action::ToggleDock, ""),
            (Action::ToggleSidebarCompact, ""),
            (Action::LeaderMode, ""),
            (Action::QuickAction, "<Control>F5"),
            (Action::ToggleBroadcastInput, "<Control><Shift>b"),
            (Action::Copy, "<Control><Shift>c"),
            (Action::CopyRecentOutput, "<Control><Shift>o"),
            (Action::CopyLastMessage, "<Control><Shift>m"),
            (Action::Paste, "<Control><Shift>v"),
            (Action::ToggleSelectionMode, "<Control><Shift>s"),
            (Action::SplitVertical, "<Control><Shift>backslash"),
            (Action::SplitHorizontal, "<Control><Shift>minus"),
            (Action::ClosePane, "<Control><Shift>w"),
            (Action::TogglePaneZoom, "<Control><Shift>z"),
            (Action::FocusPaneLeft, "<Control><Shift>h"),
            (Action::FocusPaneRight, "<Control><Shift>l"),
            (Action::FocusPaneUp, "<Control><Shift>k"),
            (Action::FocusPaneDown, "<Control><Shift>j"),
            (Action::JumpPreviousPrompt, "<Control><Shift>Up"),
            (Action::JumpNextPrompt, "<Control><Shift>Down"),
            (Action::NewWorkspace, ""),
            (Action::PreviousWorkspace, "<Control><Alt>Tab"),
            (Action::NextWorkspace, "<Control>Page_Down"),
            (Action::PrevWorkspace, "<Control>Page_Up"),
            (Action::DiscoverTab, "<Control><Shift>d"),
            (Action::SendToPane, "<Control><Shift>e"),
            (Action::ClipboardHistory, "<Control><Shift>y"),
            (Action::RecentFiles, "<Control><Shift>r"),
            (Action::PeekFile, "<Control><Shift>space"),
            // Adopted into the typed action vocabulary so the palette and the
            // sidebar/terminal menus can reach them without raw GAction strings.
            // Shipped unbound — adding them changes no user's existing chords.
            (Action::BroadcastTab, ""),
            (Action::BroadcastWorkspace, ""),
            (Action::BroadcastOff, ""),
            (Action::RelayLastMessage, ""),
            (Action::SendLastOutputToPane, ""),
        ]
    }

    fn complete_window_action_registrations() -> Vec<(Action, &'static str)> {
        Action::ALL
            .iter()
            .copied()
            .filter_map(|action| {
                action
                    .gaction_name()
                    .strip_prefix("win.")
                    .map(|name| (action, name))
            })
            .collect()
    }

    #[test]
    fn test_action_registration_missing_registration_fails_contract() {
        let mut registrations = complete_window_action_registrations();
        registrations.retain(|(action, _)| *action != Action::NewTab);

        let error = validate_window_action_registrations(&registrations)
            .expect_err("removing a handler must fail the action contract");
        assert!(error.contains("new-tab"), "unexpected error: {error}");
    }

    #[test]
    fn test_action_registration_renamed_registration_fails_contract() {
        let mut registrations = complete_window_action_registrations();
        let entry = registrations
            .iter_mut()
            .find(|(action, _)| *action == Action::NewTab)
            .expect("NewTab must be a window action");
        entry.1 = "renamed-new-tab";

        let error = validate_window_action_registrations(&registrations)
            .expect_err("renaming a handler must fail the action contract");
        assert!(error.contains("new-tab"), "unexpected error: {error}");
    }

    #[test]
    fn test_action_registration_duplicate_registration_fails_contract() {
        let mut registrations = complete_window_action_registrations();
        registrations.push((Action::NewTab, "new-tab"));

        let error = validate_window_action_registrations(&registrations)
            .expect_err("registering an action twice must fail the action contract");
        assert!(error.contains("duplicate"), "unexpected error: {error}");
    }

    #[test]
    fn test_action_registration_empty_registration_fails_contract() {
        let error = validate_window_action_registrations(&[])
            .expect_err("an empty action registration set must fail the contract");
        assert!(error.contains("no bindable"), "unexpected error: {error}");
    }

    #[test]
    fn action_registration_rejects_raw_activation_mutation_in_lib() {
        let source = r#"
            gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.shortcut-help",
                None,
            );
        "#;

        assert_eq!(
            untyped_gtk_activations(Path::new("lib.rs"), source).len(),
            1
        );
    }

    #[test]
    fn action_registration_rejects_indirect_activation_mutation_in_another_ui_module() {
        let source = r#"
            let action_name = action.gaction_name();
            gtk::prelude::WidgetExt::activate_action(&window, action_name, None);
        "#;

        assert_eq!(
            untyped_gtk_activations(Path::new("sidebar.rs"), source).len(),
            1
        );
    }

    #[test]
    fn action_registration_accepts_only_the_parameterized_focus_pane_helper() {
        let source = r#"
            gio::prelude::ActionGroupExt::activate_action(
                &application,
                "focus-pane",
                Some(&value.to_variant()),
            );
        "#;

        assert!(untyped_gtk_activations(Path::new("history_view/ui.rs"), source).is_empty());
    }

    #[test]
    fn action_registration_live_rust_ui_sources_have_no_untyped_activation() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths = Vec::new();
        rust_sources(&source_root, &mut paths);

        let violations: Vec<_> = paths
            .iter()
            .flat_map(|path| {
                let source = std::fs::read_to_string(path).expect("read Rust source");
                untyped_gtk_activations(path, &source)
                    .into_iter()
                    .map(move |call| format!("{}: {call}", path.display()))
            })
            .collect();

        assert!(
            violations.is_empty(),
            "untyped GTK activations: {violations:#?}"
        );
    }

    #[test]
    fn action_roundtrips_through_display_and_from_str() {
        for &action in Action::ALL {
            let s = action.to_string();
            let parsed: Action = s.parse().unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(action, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn unknown_action_returns_error() {
        assert!("nonexistent".parse::<Action>().is_err());
    }

    #[test]
    fn defaults_cover_all_actions() {
        let cfg = KeybindingConfig::defaults();
        for &action in Action::ALL {
            let trigger = cfg.trigger(action);
            // These actions are intentionally command-palette/keybinding only by default.
            if !matches!(
                action,
                Action::NewWorkspace
                    | Action::LeaderMode
                    | Action::ToggleDock
                    | Action::ToggleSidebarCompact
                    // Ship unbound: these were palette-only before typed dispatch,
                    // so binding them by default would claim keys users already use.
                    | Action::BroadcastTab
                    | Action::BroadcastWorkspace
                    | Action::BroadcastOff
                    | Action::RelayLastMessage
                    | Action::SendLastOutputToPane
            ) {
                assert!(
                    !trigger.is_empty(),
                    "action {} has no default trigger",
                    action
                );
            }
        }
    }

    #[test]
    fn default_chord_map_has_five_entries() {
        let map = default_quick_action_map();
        assert_eq!(map.len(), 5);
        assert_eq!(map[&'d'], WorkspaceAction::Dev);
        assert_eq!(map[&'t'], WorkspaceAction::Test);
    }

    #[test]
    fn parse_valid_toml_overrides_defaults() {
        let toml_str = r#"
[shortcuts]
new-tab = "<Control><Shift>t"
copy = "none"
leader-mode = "<Control>b"

[chords.quick-action]
x = "test"

[chords.leader]
c = "new-tab"
"#;
        let raw: RawKeybindingFile = toml::from_str(toml_str).unwrap();
        assert_eq!(raw.shortcuts["new-tab"], "<Control><Shift>t");
        assert_eq!(raw.shortcuts["copy"], "none");
        assert_eq!(raw.shortcuts["leader-mode"], "<Control>b");
        assert_eq!(
            raw.chords.as_ref().unwrap().quick_action.as_ref().unwrap()["x"],
            "test"
        );
        assert_eq!(
            raw.chords.as_ref().unwrap().leader.as_ref().unwrap()["c"],
            "new-tab"
        );
    }

    #[test]
    fn parse_unknown_action_in_toml_is_captured() {
        let toml_str = r#"
[shortcuts]
nonexistent = "<Control>q"
"#;
        let raw: RawKeybindingFile = toml::from_str(toml_str).unwrap();
        assert!(raw.shortcuts.contains_key("nonexistent"));
        // The Action::from_str will reject this — tested separately
        assert!("nonexistent".parse::<Action>().is_err());
    }

    fn write_temp_keybindings(contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static ID: AtomicU64 = AtomicU64::new(0);
        let id = ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "taarof-kb-validate-{}-{}.toml",
            std::process::id(),
            id
        ));
        std::fs::write(&path, contents).expect("write temp keybindings");
        path
    }

    #[test]
    fn validate_keybindings_flags_unknown_action() {
        let path = write_temp_keybindings("[shortcuts]\nnonexistent = \"<Control>q\"\n");
        let findings = validate_keybindings_file(&path);
        std::fs::remove_file(&path).ok();
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("unknown action") && f.message.contains("nonexistent")),
            "expected an unknown-action finding, got: {:?}",
            findings
        );
    }

    #[test]
    fn validate_keybindings_flags_invalid_trigger() {
        let path = write_temp_keybindings("[shortcuts]\nnew-tab = \"<Nonsense>garbage\"\n");
        let findings = validate_keybindings_file(&path);
        std::fs::remove_file(&path).ok();
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("invalid trigger")),
            "expected an invalid-trigger finding, got: {:?}",
            findings
        );
    }

    #[test]
    fn validate_keybindings_accepts_valid_file() {
        let path =
            write_temp_keybindings("[shortcuts]\nnew-tab = \"<Control>t\"\ncopy = \"none\"\n");
        let findings = validate_keybindings_file(&path);
        std::fs::remove_file(&path).ok();
        assert!(
            findings.is_empty(),
            "expected no findings, got: {:?}",
            findings
        );
    }

    #[test]
    fn validate_keybindings_missing_file_is_ok() {
        let path = std::env::temp_dir().join("taarof-kb-definitely-missing-xyz.toml");
        assert!(validate_keybindings_file(&path).is_empty());
    }

    #[test]
    fn followup_key_normalizes_letters_only() {
        assert_eq!(parse_followup_key("D"), Some('d'));
        assert_eq!(parse_followup_key("%"), Some('%'));
        assert_eq!(parse_followup_key("ab"), None);
    }

    #[test]
    fn leader_target_validation_rejects_nested_modes() {
        assert!(leader_target_allowed(Action::NewTab));
        assert!(!leader_target_allowed(Action::LeaderMode));
        assert!(!leader_target_allowed(Action::QuickAction));
    }

    #[test]
    fn release_shortcut_matrix_includes_release_critical_actions() {
        assert_eq!(release_shortcut_contract().len(), Action::ALL.len());

        for &(action, trigger) in release_shortcut_contract() {
            assert_eq!(
                action.default_trigger(),
                trigger,
                "default trigger changed for {:?}",
                action
            );
        }

        assert_eq!(Action::LeaderMode.default_trigger(), "");
        assert_eq!(Action::NewWorkspace.default_trigger(), "");
    }

    #[test]
    fn release_shortcut_matrix_has_no_duplicate_default_triggers() {
        let mut by_trigger: BTreeMap<&str, Vec<Action>> = BTreeMap::new();
        for &(action, trigger) in release_shortcut_contract() {
            if trigger.is_empty() {
                continue;
            }
            by_trigger.entry(trigger).or_default().push(action);
        }

        let duplicates: Vec<String> = by_trigger
            .into_iter()
            .filter(|(_, actions)| actions.len() > 1)
            .map(|(trigger, actions)| format!("{trigger}: {:?}", actions))
            .collect();

        assert!(
            duplicates.is_empty(),
            "duplicate default triggers found: {}",
            duplicates.join(", ")
        );
    }

    /// Build a config from a `[shortcuts]` TOML table *without* the GTK-based
    /// `is_valid_trigger` check, so resolver tests run headless. Starts from
    /// defaults and overlays each provided override (empty/"none"/"disabled"
    /// unbinds), mirroring `load_shortcuts`'s overlay semantics.
    fn config_from_toml(toml_str: &str) -> KeybindingConfig {
        let raw: RawKeybindingFile = toml::from_str(toml_str).unwrap();
        let mut cfg = KeybindingConfig::defaults();
        for (key, value) in &raw.shortcuts {
            let action = key.parse::<Action>().expect("known action in test toml");
            if value.is_empty() || value == "none" || value == "disabled" {
                cfg.shortcuts.insert(action, String::new());
            } else {
                cfg.shortcuts.insert(action, value.clone());
            }
        }
        cfg
    }

    #[test]
    fn shortcut_help_defaults_to_question_and_supports_override_and_unbind() {
        assert_eq!(
            KeybindingConfig::defaults().trigger(Action::ShortcutHelp),
            "question"
        );

        let overridden = config_from_toml(
            r#"
[shortcuts]
shortcut-help = "<Control>question"
"#,
        );
        assert_eq!(
            overridden.trigger(Action::ShortcutHelp),
            "<Control>question"
        );

        let unbound = config_from_toml(
            r#"
[shortcuts]
shortcut-help = "none"
"#,
        );
        assert_eq!(unbound.trigger(Action::ShortcutHelp), "");
        assert_eq!(Action::ShortcutHelp.scope(), ActionScope::Window);
    }

    #[test]
    fn effective_bindings_cover_every_action_once() {
        let cfg = KeybindingConfig::defaults();
        let rows = cfg.effective_bindings();
        assert_eq!(rows.len(), Action::ALL.len());
        for &action in Action::ALL {
            assert_eq!(
                rows.iter().filter(|r| r.action == action).count(),
                1,
                "action {action} must appear exactly once"
            );
        }
    }

    #[test]
    fn effective_bindings_shows_default_trigger_and_not_customized() {
        let cfg = KeybindingConfig::defaults();
        let rows = cfg.effective_bindings();
        let new_tab = rows
            .iter()
            .find(|r| r.action == Action::NewTab)
            .expect("NewTab row present");
        assert!(
            !new_tab.customized,
            "default binding must not be flagged customized"
        );
        // Ctrl+T should render as a non-empty human label.
        let label = new_tab
            .trigger
            .as_deref()
            .expect("NewTab has a default trigger");
        assert!(
            label.to_lowercase().contains('t'),
            "unexpected label: {label}"
        );
        assert_eq!(new_tab.display_name, "New Tab");
    }

    #[test]
    fn effective_bindings_marks_override_as_customized() {
        let cfg = config_from_toml("[shortcuts]\nnew-tab = \"<Control><Alt>n\"\n");
        let rows = cfg.effective_bindings();
        let new_tab = rows
            .iter()
            .find(|r| r.action == Action::NewTab)
            .expect("NewTab row present");
        assert!(
            new_tab.customized,
            "rebound action must be flagged customized"
        );
        assert!(new_tab.trigger.is_some());

        // An untouched action stays non-customized.
        let copy = rows.iter().find(|r| r.action == Action::Copy).unwrap();
        assert!(!copy.customized);
    }

    #[test]
    fn effective_bindings_unbound_action_has_no_trigger() {
        // NewWorkspace is unbound by default.
        let cfg = KeybindingConfig::defaults();
        let rows = cfg.effective_bindings();
        let new_ws = rows
            .iter()
            .find(|r| r.action == Action::NewWorkspace)
            .expect("NewWorkspace row present");
        assert!(
            new_ws.trigger.is_none(),
            "unbound action must have no trigger"
        );
        assert!(!new_ws.customized);

        // Explicitly unbinding a normally-bound action is a customization with
        // no trigger.
        let unbound = config_from_toml("[shortcuts]\ncopy = \"none\"\n");
        let copy = unbound
            .effective_bindings()
            .into_iter()
            .find(|r| r.action == Action::Copy)
            .unwrap();
        assert!(copy.trigger.is_none());
        assert!(
            copy.customized,
            "explicit unbind differs from default → customized"
        );
    }

    #[test]
    fn effective_bindings_grouped_by_category_in_order() {
        let cfg = KeybindingConfig::defaults();
        let rows = cfg.effective_bindings();
        // Category ordinal must be non-decreasing across the ordered rows.
        let mut last = ActionCategory::General;
        for row in &rows {
            assert!(
                row.category >= last,
                "rows not ordered by category: {:?} after {:?}",
                row.category,
                last
            );
            last = row.category;
        }
    }

    #[test]
    fn every_category_is_reachable_from_some_action() {
        for &cat in ActionCategory::ALL {
            assert!(
                Action::ALL.iter().any(|a| a.category() == cat),
                "category {:?} has no actions",
                cat
            );
        }
    }

    #[test]
    fn hint_lines_include_expected_default_triggers() {
        let cfg = KeybindingConfig::defaults();
        let lines = cfg.hint_lines();
        assert_eq!(lines.len(), 5);

        let joined = lines.join("\n").to_lowercase();
        assert!(
            joined.contains("new tab"),
            "hints missing new tab: {lines:?}"
        );
        assert!(
            joined.contains("command palette"),
            "hints missing palette: {lines:?}"
        );

        // New tab hint carries Ctrl+T; command palette hint carries Ctrl+Shift+P.
        let new_tab_line = lines
            .iter()
            .find(|l| l.to_lowercase().starts_with("new tab"))
            .unwrap();
        assert!(
            new_tab_line.contains('·'),
            "hint must pair action with trigger: {new_tab_line}"
        );
        assert!(new_tab_line.to_lowercase().contains("ctrl"));
    }

    #[test]
    fn hint_lines_reflect_custom_binding() {
        let cfg = config_from_toml("[shortcuts]\ncommand-palette = \"<Control><Alt>k\"\n");
        let lines = cfg.hint_lines();
        let palette_line = lines
            .iter()
            .find(|l| l.to_lowercase().starts_with("command palette"))
            .unwrap();
        // Custom binding is Ctrl+Alt+K; default Ctrl+Shift+P must be gone.
        assert!(
            palette_line.to_lowercase().contains("alt"),
            "line: {palette_line}"
        );
        assert!(
            !palette_line.to_lowercase().contains("shift"),
            "stale default in: {palette_line}"
        );
    }

    #[test]
    fn hint_lines_handle_unbound_action_without_trigger() {
        // Unbind command-palette entirely; the hint text still lists it.
        let cfg = config_from_toml("[shortcuts]\ncommand-palette = \"none\"\n");
        let lines = cfg.hint_lines();
        let palette_line = lines
            .iter()
            .find(|l| l.to_lowercase().starts_with("command palette"))
            .unwrap();
        assert!(
            !palette_line.contains('·'),
            "unbound hint must omit trigger separator: {palette_line}"
        );
    }

    #[test]
    fn release_shortcut_matrix_scopes_terminal_actions_correctly() {
        let expected_terminal_scope = [
            Action::Copy,
            Action::CopyRecentOutput,
            Action::CopyLastMessage,
            Action::Paste,
            Action::ToggleSelectionMode,
            Action::SplitVertical,
            Action::SplitHorizontal,
            Action::ClosePane,
            Action::TogglePaneZoom,
            Action::FocusPaneLeft,
            Action::FocusPaneRight,
            Action::FocusPaneUp,
            Action::FocusPaneDown,
        ];

        for action in expected_terminal_scope {
            assert_eq!(
                action.scope(),
                ActionScope::Terminal,
                "{:?} must remain terminal-scoped",
                action
            );
        }
    }
}
