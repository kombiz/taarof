use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Debug)]
enum ConfigError {
    Toml(toml::de::Error),
    Invalid {
        errors: Vec<String>,
        config: Box<AppConfig>,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toml(error) => error.fmt(formatter),
            Self::Invalid { errors, .. } => write!(formatter, "{}", errors.join("; ")),
        }
    }
}

impl std::error::Error for ConfigError {}

static LIVE_CONFIG_SNAPSHOT: OnceLock<RwLock<LiveConfigSnapshot>> = OnceLock::new();

#[derive(Debug, Clone, Default)]
pub struct LiveConfigSnapshot {
    pub app: AppConfig,
    pub ghostty: GhosttyConfig,
}

impl LiveConfigSnapshot {
    pub fn load() -> Self {
        Self {
            app: AppConfig::load(),
            ghostty: GhosttyConfig::load(),
        }
    }

    #[cfg(test)]
    fn load_from_paths(app_config_path: &Path, ghostty_config_path: &Path) -> Self {
        Self {
            app: AppConfig::load_from_path(app_config_path),
            ghostty: GhosttyConfig::load_from_path(ghostty_config_path),
        }
    }
}

/// What happens when a tmux-backed pane is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TmuxCloseBehavior {
    #[default]
    Close,
    Detach,
}

/// The user-facing consequence of closing a tmux-backed pane/tab, decided from
/// the configured `close_behavior`. This is a pure decision so the GTK layer can
/// stay a thin translation into a dialog/toast, and the logic stays testable
/// headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxCloseConsequence {
    /// Behavior is `close`: closing kills the tmux session. Confirm first and
    /// offer detach as the non-destructive alternative.
    ConfirmKill,
    /// Behavior is `detach`: closing leaves the session running in the
    /// background. No confirmation needed; a toast can reassure the user.
    DetachSurvives,
}

/// Decide what closing a tmux-backed pane means for the user.
///
/// `is_tmux_backed` is whether the pane/tab actually has a live tmux session
/// behind it. Non-backed panes never reach here in a meaningful way, but the
/// caller may pass `false`, in which case we still report `DetachSurvives`
/// (closing a plain pane destroys nothing tmux-owned).
pub fn tmux_close_consequence(
    behavior: TmuxCloseBehavior,
    is_tmux_backed: bool,
) -> TmuxCloseConsequence {
    match (behavior, is_tmux_backed) {
        (TmuxCloseBehavior::Close, true) => TmuxCloseConsequence::ConfirmKill,
        _ => TmuxCloseConsequence::DetachSurvives,
    }
}

/// Whether creating a tmux-backed pane in this environment risks a confusing
/// nested-tmux experience.
///
/// Two independent triggers:
/// - `app_inside_tmux`: taarof itself is running inside a tmux client (its own
///   `$TMUX` was set at startup), so a new local tmux-backed pane nests tmux.
/// - `over_ssh`: the pane targets a remote host over SSH, where the remote shell
///   may already have `$TMUX` set (its own tmux server), also nesting.
pub fn tmux_nesting_warning(app_inside_tmux: bool, over_ssh: bool) -> bool {
    app_inside_tmux || over_ssh
}

/// Whether taarof itself was launched from within a tmux client (its own
/// `$TMUX` is set). Read once at process start into a cell so later child
/// environments (which taarof may strip `$TMUX` from) don't change the answer.
pub fn app_launched_inside_tmux() -> bool {
    use std::sync::OnceLock;
    static INSIDE: OnceLock<bool> = OnceLock::new();
    *INSIDE.get_or_init(|| std::env::var_os("TMUX").is_some())
}

/// Whether a newly created tab or split should inherit tmux backing.
///
/// This is the single rule behind "tmux-backed workspace" inheritance: a new
/// pane is tmux-backed when the workspace opts in (`workspace_tmux_backed`) OR
/// the pane it splits from is already tmux-backed (`source_pane_tmux_backed`),
/// AND the automatic tmux path is enabled globally (`tmux_enabled`). For a plain
/// new tab (not a split) pass `source_pane_tmux_backed = false`.
pub fn tmux_inherits_backing(
    workspace_tmux_backed: bool,
    source_pane_tmux_backed: bool,
    tmux_enabled: bool,
) -> bool {
    (workspace_tmux_backed || source_pane_tmux_backed) && tmux_enabled
}

#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub enabled: bool,
    pub session_prefix: String,
    pub close_behavior: TmuxCloseBehavior,
    /// How taarof styles the tmux sessions it creates. Defaults to
    /// [`crate::tmux::TmuxSessionStyle::Inherit`] so existing users see no
    /// change.
    pub session_style: crate::tmux::TmuxSessionStyle,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_prefix: "taarof".into(),
            close_behavior: TmuxCloseBehavior::Close,
            session_style: crate::tmux::TmuxSessionStyle::Inherit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub enabled: bool,
    pub port: u16,
    pub bind_address: String,
    pub unsafe_allow_non_loopback: bool,
}

pub type HistoryConfig = crate::history::HistoryConfig;

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7800,
            bind_address: "127.0.0.1".into(),
            unsafe_allow_non_loopback: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HttpControlConfig {
    pub enabled: bool,
}

/// Config-driven integration with an external "loop runner" CLI that can build a
/// `.plan` task into a PR (`dev_command`) or review/merge a PR (`pr_command`).
///
/// The command templates keep the integration generic — no specific tool name is
/// baked into taarof — so the feature survives the future public export. Templates
/// support `{repo}` / `{issue}` / `{pr}` / `{loop}` placeholders.
#[derive(Debug, Clone)]
pub struct LoopRunnerConfig {
    pub enabled: bool,
    pub dev_command: String,
    pub pr_command: String,
    /// Loop name for the non-merging "Review only" action.
    pub review_loop: String,
    /// Loop name for the merge-capable "Review & merge" action.
    pub merge_loop: String,
}

impl Default for LoopRunnerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dev_command: "loop-runner dev-loop --repo {repo} --issue {issue}".to_string(),
            pr_command: "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}".to_string(),
            review_loop: "pr-review-readonly".to_string(),
            merge_loop: "pr-review".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskPanelDefaultView {
    #[default]
    Tasks,
    PullRequests,
}

/// Opt-in `.plan/tasks.json` task-tracking UI for the right-hand Tasks panel.
/// Off by default so workspaces that happen to contain `.plan` directories
/// don't surface unrelated task backlogs.
#[derive(Debug, Clone, Default)]
pub struct TasksConfig {
    pub enabled: bool,
    pub pull_requests: bool,
    pub default_view: TaskPanelDefaultView,
}

#[derive(Debug, Clone, Default)]
pub struct DockConfig {
    pub visible: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub auto_resume_agents: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateConfig {
    pub installed_binary_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    pub appearance: AppearanceConfig,
    pub sidebar: SidebarConfig,
    pub terminal: TerminalConfig,
    pub mise: MiseConfig,
    pub tmux: TmuxConfig,
    pub hosts: Vec<crate::host::HostConfig>,
    pub http: HttpConfig,
    pub history: HistoryConfig,
    pub http_control: HttpControlConfig,
    pub tasks: TasksConfig,
    pub dock: DockConfig,
    pub session: SessionConfig,
    pub update: UpdateConfig,
    pub editor: EditorConfig,
    pub browser: BrowserConfig,
    pub loop_runner: LoopRunnerConfig,
}

impl AppConfig {
    /// Load from ~/.config/taarof/config.toml.
    pub fn load() -> Self {
        Self::load_from_path(&default_app_config_path())
    }

    fn load_from_path(path: &Path) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(_) => return Self::default(),
        };

        match parse_app_config_from_str(&contents) {
            Ok(config) => config,
            Err(ConfigError::Toml(err)) => {
                eprintln!("taarof: failed to parse config.toml: {err}");
                let reason = format!(
                    "config.toml could not be parsed, so history retention could not be validated: {err}"
                );
                crate::diagnostics::record_config_error(reason.clone(), None);
                let mut config = Self::default();
                config.history.config_error = Some(reason);
                config
            }
            Err(ConfigError::Invalid { errors, mut config }) => {
                for error in &errors {
                    eprintln!("taarof: invalid config.toml: {error}");
                }
                crate::diagnostics::record_config_error(
                    "history configuration is invalid",
                    Some(serde_json::json!({"errors": errors})),
                );
                config.history.config_error = Some(errors.join("; "));
                *config
            }
        }
    }
}

/// Resolved path to `config.toml` (`~/.config/taarof/config.toml`).
pub fn app_config_path() -> PathBuf {
    default_app_config_path()
}

fn default_app_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("taarof/config.toml")
}

fn default_ghostty_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ghostty/config")
}

/// Optional full-app colour-role overrides. The shipped Majlis values live in
/// `resources/style.css`; config values replace those role tokens (including
/// their alpha-channel uses) before GTK parses the stylesheet.
#[derive(Debug, Clone, Default)]
pub struct AppearanceConfig {
    pub base: Option<String>,
    pub mantle: Option<String>,
    pub crust: Option<String>,
    pub surface: Option<String>,
    pub surface_raised: Option<String>,
    pub border: Option<String>,
    pub text: Option<String>,
    pub subtext: Option<String>,
    pub muted: Option<String>,
    pub title: Option<String>,
    pub soft_text: Option<String>,
    pub warm_muted: Option<String>,
    pub accent: Option<String>,
    pub accent_hover: Option<String>,
    pub running: Option<String>,
    pub activity: Option<String>,
    pub waiting: Option<String>,
    pub ports: Option<String>,
    pub error: Option<String>,
}

impl AppearanceConfig {
    fn apply_to_css(&self, base_css: &str) -> String {
        let mut css = base_css.to_string();
        let mut replacements = Vec::new();
        for (index, (default, replacement)) in [
            ("#14110c", self.base.as_deref()),
            ("#100d09", self.mantle.as_deref()),
            ("#0b0906", self.crust.as_deref()),
            ("#241d13", self.surface.as_deref()),
            ("#34291a", self.surface_raised.as_deref()),
            ("#463726", self.border.as_deref()),
            ("#efe7d6", self.text.as_deref()),
            ("#c3b9a7", self.subtext.as_deref()),
            ("#7c715e", self.muted.as_deref()),
            ("#f6f0e4", self.title.as_deref()),
            ("#d8cdb6", self.soft_text.as_deref()),
            ("#a08d6f", self.warm_muted.as_deref()),
            ("#e0a43a", self.accent.as_deref()),
            ("#f0be5e", self.accent_hover.as_deref()),
            ("#7bbf7a", self.running.as_deref()),
            ("#6bb0b8", self.activity.as_deref()),
            ("#d9b34a", self.waiting.as_deref()),
            ("#e0913a", self.ports.as_deref()),
            ("#d6796b", self.error.as_deref()),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(replacement) = replacement.filter(|value| is_six_digit_hex(value)) else {
                continue;
            };
            let color_placeholder = format!("__TAAROF_COLOR_{index}__");
            css = css.replace(default, &color_placeholder);

            let rgb_replacement = match (rgb_channels(default), rgb_channels(replacement)) {
                (Some(default_rgb), Some(replacement_rgb)) => {
                    let rgb_placeholder = format!("__TAAROF_RGB_{index}__");
                    css = css.replace(&default_rgb, &rgb_placeholder);
                    Some((rgb_placeholder, replacement_rgb))
                }
                _ => None,
            };
            replacements.push((color_placeholder, replacement.to_string(), rgb_replacement));
        }

        // Resolve placeholders only after every default token has been removed,
        // so choosing another Majlis default as an override cannot cascade into
        // a different role's replacement.
        for (color_placeholder, replacement, rgb_replacement) in replacements {
            css = css.replace(&color_placeholder, &replacement);
            if let Some((rgb_placeholder, replacement_rgb)) = rgb_replacement {
                css = css.replace(&rgb_placeholder, &replacement_rgb);
            }
        }
        css
    }
}

fn is_six_digit_hex(value: &str) -> bool {
    value
        .strip_prefix('#')
        .is_some_and(|hex| hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
}

fn rgb_channels(value: &str) -> Option<String> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(format!("{red}, {green}, {blue}"))
}

#[derive(Debug, Clone)]
pub struct SidebarConfig {
    pub position: SidebarPosition,
    pub compact: bool,
    pub theme: SidebarTheme,
    pub workspace_icons: WorkspaceIconConfig,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            position: SidebarPosition::Left,
            compact: false,
            theme: SidebarTheme::default(),
            workspace_icons: WorkspaceIconConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarPosition {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Default)]
pub struct SidebarTheme {
    pub background: Option<String>,
    pub surface: Option<String>,
    pub accent: Option<String>,
    pub text: Option<String>,
    pub muted: Option<String>,
}

impl SidebarTheme {
    fn has_overrides(&self) -> bool {
        self.background.is_some()
            || self.surface.is_some()
            || self.accent.is_some()
            || self.text.is_some()
            || self.muted.is_some()
    }

    fn override_css_with_appearance(&self, appearance: &AppearanceConfig) -> Option<String> {
        if !self.has_overrides() {
            return None;
        }

        let background = self
            .background
            .as_deref()
            .or(appearance.mantle.as_deref())
            .unwrap_or("#100d09");
        let surface = self
            .surface
            .as_deref()
            .or(appearance.surface.as_deref())
            .unwrap_or("#241d13");
        let accent = self
            .accent
            .as_deref()
            .or(appearance.accent.as_deref())
            .unwrap_or("#e0a43a");
        let text = self
            .text
            .as_deref()
            .or(appearance.text.as_deref())
            .unwrap_or("#efe7d6");
        let muted = self
            .muted
            .as_deref()
            .or(appearance.muted.as_deref())
            .unwrap_or("#7c715e");
        let active_fg = self
            .background
            .as_deref()
            .or(appearance.base.as_deref())
            .unwrap_or("#14110c");

        Some(format!(
            r#"
.sidebar {{
    background-color: {background};
    color: {text};
}}

.sidebar-mark,
.ws-icon,
.session-dot {{
    color: {accent};
}}

.sidebar-divider,
paned > separator {{
    background-color: {surface};
}}

.workspace-header,
.tab-row,
.session-info,
.new-tab-button {{
    border-color: {surface};
}}

.workspace-header,
.tab-row {{
    background-color: {background};
}}

.session-info {{
    background-color: {surface};
}}

.workspace-header:hover,
.tab-row:hover,
.new-tab-button:hover,
.workspace-action-button,
.workspace-action-button:hover,
popover.menu modelbutton:hover {{
    background-color: {surface};
}}

.tab-row:hover {{
    border-left-color: {accent};
}}

.sidebar-section-label,
.tab-cwd,
.tab-branch,
.ws-chevron,
.tracking-id {{
    color: {muted};
}}

.tab-search-entry,
.tab-search-entry > text {{
    background-color: {background};
    color: {text};
    border-color: {surface};
}}

.tab-search-entry:focus-visible,
.tab-search-entry:focus-within {{
    border-color: {accent};
    box-shadow: 0 0 0 1px {accent};
}}

.tab-number,
.ws-tab-count {{
    color: {text};
    background-color: {background};
    border-color: {surface};
}}

.sidebar-version,
.ws-tool-version-chip {{
    color: {accent};
    background-color: {background};
    border-color: {accent};
}}

.sidebar-logo,
.tab-button,
.tab-name,
.workspace-header .ws-name,
.session-name,
.tracking-title,
.new-tab-button:hover {{
    color: {text};
}}

.new-tab-button {{
    color: {muted};
}}

.tab-rename-entry,
popover.menu {{
    background-color: {background};
    color: {text};
    border-color: {accent};
}}

.tab-row.active {{
    background-color: {accent};
    border-color: {accent};
}}

.tab-row.active:hover {{
    background-color: {accent};
    border-color: {accent};
}}

.workspace-header.active {{
    background-color: {surface};
    border-color: {accent};
}}

.tab-row.active .tab-button,
.tab-row.active .tab-name,
.tab-row.active .tab-number,
.tab-row.active .tab-cwd,
.tab-row.active .tab-branch,
.tab-row.active .tab-ports,
.tab-row.active .tab-activity,
.tab-row.active .agent-dot,
.tab-row.active:hover .tab-close,
.tab-row.active:hover .tab-close:hover {{
    color: {active_fg};
}}

.workspace-header.active .ws-name,
.workspace-header.active .ws-chevron,
.workspace-header.active .ws-icon,
.workspace-header.active .ws-tab-count {{
    color: {text};
}}

.workspace-header.active .ws-tool-version-chip {{
    color: {accent};
    background-color: {background};
    border-color: {accent};
}}

.tab-row.active .tab-button,
.workspace-header.active .ws-name {{
    text-shadow: none;
}}

.ws-worktree-badge,
.tracking-status-in-progress {{
    color: {accent};
    background-color: {background};
    border-color: {accent};
}}

.workspace-header.active .ws-worktree-badge {{
    color: {accent};
    background-color: {background};
    border-color: {accent};
}}

.tab-row.active .tab-number,
.workspace-header.active .ws-tab-count {{
    background-color: {background};
    border-color: {accent};
}}
"#
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceIconConfig {
    pub default: Option<String>,
    pub worktree: Option<String>,
    pub by_name: BTreeMap<String, String>,
    pub by_repo: BTreeMap<String, String>,
}

impl WorkspaceIconConfig {
    fn resolve(&self, workspace_name: &str, repo_root: Option<&str>, is_worktree: bool) -> String {
        if let Some(icon) = find_icon_override(&self.by_name, workspace_name) {
            return icon;
        }

        if let Some(repo_name) = repo_root.and_then(path_basename) {
            if let Some(icon) = find_icon_override(&self.by_repo, repo_name) {
                return icon;
            }
        }

        if is_worktree {
            if let Some(icon) = self.worktree.clone() {
                return icon;
            }
        }

        if let Some(icon) = self.default.clone() {
            return icon;
        }

        derive_workspace_icon(workspace_name, repo_root)
    }
}

#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub copy_recent_lines: u32,
    pub clipboard_history_size: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            copy_recent_lines: 200,
            clipboard_history_size: 50,
        }
    }
}

/// What a Ctrl+Click on a `path:line` terminal match should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClickAction {
    /// Open the file in the in-app peek overlay (fast, read-only). Default.
    #[default]
    Peek,
    /// Eject straight to the configured external editor.
    Editor,
}

#[derive(Debug, Clone)]
pub struct EditorConfig {
    pub open_command: String,
    /// Where a click-to-open file target lands: peek overlay or external editor.
    pub click_action: ClickAction,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            open_command: "zed {path}:{line}:{col}".to_string(),
            click_action: ClickAction::Peek,
        }
    }
}

/// Which browser opens external URLs (terminal links, task-panel PR links).
#[derive(Debug, Clone, Default)]
pub struct BrowserConfig {
    /// Command template with a `{url}` placeholder. `None` opens URLs with the
    /// system default browser via GIO.
    pub open_command: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MiseConfig {
    pub include_global: bool,
}

pub fn install_app_config(config: &AppConfig) {
    let ghostty = ghostty_config();
    install_live_config(LiveConfigSnapshot {
        app: config.clone(),
        ghostty,
    });
}

pub fn install_live_config(snapshot: LiveConfigSnapshot) {
    let store = LIVE_CONFIG_SNAPSHOT.get_or_init(|| RwLock::new(LiveConfigSnapshot::default()));
    let mut guard = store.write().unwrap_or_else(|err| err.into_inner());
    *guard = snapshot;
}

pub fn reload_live_config() -> LiveConfigSnapshot {
    let snapshot = LiveConfigSnapshot::load();
    install_live_config(snapshot.clone());
    snapshot
}

#[derive(Debug)]
enum CapturedAppConfig {
    Missing,
    Contents(String),
}

fn capture_app_config(path: &Path) -> Result<CapturedAppConfig, Vec<Finding>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(CapturedAppConfig::Contents(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(CapturedAppConfig::Missing),
        Err(err) => Err(vec![Finding::error(
            "config.toml",
            format!("could not read {}: {err}", path.display()),
        )]),
    }
}

fn validated_live_config_from_capture(
    captured: CapturedAppConfig,
    ghostty_path: &Path,
) -> Result<LiveConfigSnapshot, Vec<Finding>> {
    let app = match captured {
        CapturedAppConfig::Missing => AppConfig::default(),
        CapturedAppConfig::Contents(contents) => {
            let findings = validate_app_config_str(&contents);
            if findings
                .iter()
                .any(|finding| finding.severity == Severity::Error)
            {
                return Err(findings);
            }
            parse_app_config_from_str(&contents).map_err(|error| {
                vec![Finding::error(
                    "config.toml",
                    format!("validated config could not be loaded: {error}"),
                )]
            })?
        }
    };

    Ok(LiveConfigSnapshot {
        app,
        ghostty: GhosttyConfig::load_from_path(ghostty_path),
    })
}

#[cfg(test)]
fn install_live_config_from_capture(
    captured: CapturedAppConfig,
    ghostty_path: &Path,
    install: impl FnOnce(&LiveConfigSnapshot),
) -> Result<LiveConfigSnapshot, Vec<Finding>> {
    let snapshot = validated_live_config_from_capture(captured, ghostty_path)?;
    install(&snapshot);
    Ok(snapshot)
}

/// Read, validate, and parse a live snapshot without installing it. This is
/// safe to run on a blocking worker; callers can marshal the finished snapshot
/// back to the GTK main context before changing process or widget state.
pub fn load_live_config_if_valid() -> Result<LiveConfigSnapshot, Vec<Finding>> {
    let captured = capture_app_config(&app_config_path())?;
    validated_live_config_from_capture(captured, &default_ghostty_config_path())
}

/// Capture config.toml once, then validate and parse that exact buffer before
/// installing it. A transient partial write, malformed edit, or read failure
/// therefore leaves the last known-good process-wide snapshot in place.
pub fn reload_live_config_if_valid() -> Result<LiveConfigSnapshot, Vec<Finding>> {
    let snapshot = load_live_config_if_valid()?;
    install_live_config(snapshot.clone());
    Ok(snapshot)
}

fn with_live_config<T>(f: impl FnOnce(&LiveConfigSnapshot) -> T) -> T {
    let store = LIVE_CONFIG_SNAPSHOT.get_or_init(|| RwLock::new(LiveConfigSnapshot::default()));
    let guard = store.read().unwrap_or_else(|err| err.into_inner());
    f(&guard)
}

pub fn sidebar_position() -> SidebarPosition {
    with_live_config(|config| config.app.sidebar.position)
}

pub fn sidebar_compact() -> bool {
    with_live_config(|config| config.app.sidebar.compact)
}

pub fn appearance_css(base_css: &str) -> String {
    with_live_config(|config| config.app.appearance.apply_to_css(base_css))
}

pub fn sidebar_override_css() -> Option<String> {
    with_live_config(|config| {
        config
            .app
            .sidebar
            .theme
            .override_css_with_appearance(&config.app.appearance)
    })
}

pub fn workspace_icon_for(
    workspace_name: &str,
    repo_root: Option<&str>,
    is_worktree: bool,
) -> String {
    with_live_config(|config| {
        config
            .app
            .sidebar
            .workspace_icons
            .resolve(workspace_name, repo_root, is_worktree)
    })
}

pub fn tmux_config() -> TmuxConfig {
    with_live_config(|config| config.app.tmux.clone())
}

/// The configured `[tmux].session_style`, read without cloning the rest of the
/// tmux config. Spawn paths call this per pane creation.
pub fn tmux_session_style() -> crate::tmux::TmuxSessionStyle {
    with_live_config(|config| config.app.tmux.session_style)
}

pub fn app_config() -> AppConfig {
    with_live_config(|config| config.app.clone())
}

pub fn should_auto_resume_agents_on_session_restore() -> bool {
    with_live_config(|config| config.app.session.auto_resume_agents)
}

pub fn terminal_config() -> TerminalConfig {
    with_live_config(|config| config.app.terminal.clone())
}

pub fn editor_config() -> EditorConfig {
    with_live_config(|config| config.app.editor.clone())
}

pub fn browser_config() -> BrowserConfig {
    with_live_config(|config| config.app.browser.clone())
}

pub fn mise_config() -> MiseConfig {
    with_live_config(|config| config.app.mise.clone())
}

pub fn ghostty_config() -> GhosttyConfig {
    with_live_config(|config| config.ghostty.clone())
}

pub fn http_config() -> HttpConfig {
    with_live_config(|config| config.app.http.clone())
}

pub fn history_config() -> HistoryConfig {
    with_live_config(|config| config.app.history.clone())
}

pub fn update_config() -> UpdateConfig {
    with_live_config(|config| config.app.update.clone())
}

pub fn http_control_config() -> HttpControlConfig {
    with_live_config(|config| config.app.http_control.clone())
}

pub fn tasks_config() -> TasksConfig {
    with_live_config(|config| config.app.tasks.clone())
}

pub fn dock_visible() -> bool {
    with_live_config(|config| config.app.dock.visible)
}

pub fn loop_runner_config() -> LoopRunnerConfig {
    with_live_config(|config| config.app.loop_runner.clone())
}

pub fn host_config(name: &str) -> Option<crate::host::HostConfig> {
    with_live_config(|config| {
        config
            .app
            .hosts
            .iter()
            .find(|host| host.name == name)
            .cloned()
    })
}

pub fn remote_hosts() -> Vec<crate::host::HostConfig> {
    with_live_config(|config| {
        config
            .app
            .hosts
            .iter()
            .filter(|host| host.ssh_target.is_some())
            .cloned()
            .collect()
    })
}

#[derive(Debug, Default, Deserialize)]
struct RawAppConfig {
    #[serde(default)]
    appearance: RawAppearanceConfig,
    #[serde(default)]
    sidebar: RawSidebarConfig,
    #[serde(default)]
    terminal: Option<RawTerminalConfig>,
    #[serde(default)]
    mise: Option<RawMiseConfig>,
    #[serde(default)]
    tmux: Option<RawTmuxConfig>,
    #[serde(default)]
    hosts: Option<BTreeMap<String, RawHostConfig>>,
    #[serde(default)]
    http: Option<RawHttpConfig>,
    #[serde(default)]
    history: Option<RawHistoryConfig>,
    #[serde(default)]
    http_control: Option<RawHttpControlConfig>,
    #[serde(default)]
    tasks: Option<RawTasksConfig>,
    #[serde(default)]
    dock: Option<RawDockConfig>,
    #[serde(default)]
    session: Option<RawSessionConfig>,
    #[serde(default)]
    update: Option<RawUpdateConfig>,
    #[serde(default)]
    editor: Option<RawEditorConfig>,
    #[serde(default)]
    browser: Option<RawBrowserConfig>,
    #[serde(default)]
    loop_runner: Option<RawLoopRunnerConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAppearanceConfig {
    base: Option<String>,
    mantle: Option<String>,
    crust: Option<String>,
    surface: Option<String>,
    #[serde(alias = "surface-raised")]
    surface_raised: Option<String>,
    border: Option<String>,
    text: Option<String>,
    subtext: Option<String>,
    muted: Option<String>,
    title: Option<String>,
    #[serde(alias = "soft-text")]
    soft_text: Option<String>,
    #[serde(alias = "warm-muted")]
    warm_muted: Option<String>,
    accent: Option<String>,
    #[serde(alias = "accent-hover")]
    accent_hover: Option<String>,
    running: Option<String>,
    activity: Option<String>,
    waiting: Option<String>,
    ports: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawSidebarConfig {
    #[serde(default)]
    position: Option<String>,
    #[serde(default)]
    compact: Option<bool>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    surface: Option<String>,
    #[serde(default)]
    accent: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    muted: Option<String>,
    #[serde(default, rename = "workspace-icons")]
    workspace_icons: RawWorkspaceIconConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawWorkspaceIconConfig {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    worktree: Option<String>,
    #[serde(default, rename = "by-name")]
    by_name: BTreeMap<String, String>,
    #[serde(default, rename = "by-repo")]
    by_repo: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTerminalConfig {
    copy_recent_lines: Option<u32>,
    clipboard_history_size: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawEditorConfig {
    open_command: Option<String>,
    #[serde(default, alias = "click-action")]
    click_action: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawBrowserConfig {
    #[serde(default, alias = "open-command")]
    open_command: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMiseConfig {
    include_global: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTmuxConfig {
    enabled: Option<bool>,
    session_prefix: Option<String>,
    close_behavior: Option<String>,
    session_style: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHostConfig {
    address: Option<String>,
    max_sessions: Option<u32>,
    warn_cpu_percent: Option<u32>,
    warn_memory_percent: Option<u32>,
    idle_detach_minutes: Option<u32>,
    tmux_backed: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHttpConfig {
    enabled: Option<bool>,
    port: Option<u16>,
    bind_address: Option<String>,
    unsafe_allow_non_loopback: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHistoryConfig {
    enabled: Option<bool>,
    max_age_days: Option<u64>,
    max_records: Option<u64>,
    max_bytes: Option<u64>,
    maintenance_interval_minutes: Option<u64>,
    queue_capacity: Option<usize>,
    record_events: Option<bool>,
    record_diagnostics: Option<bool>,
    record_work: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawHttpControlConfig {
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTasksConfig {
    enabled: Option<bool>,
    #[serde(alias = "pull-requests")]
    pull_requests: Option<bool>,
    #[serde(alias = "default-view")]
    default_view: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLoopRunnerConfig {
    enabled: Option<bool>,
    #[serde(alias = "dev-command")]
    dev_command: Option<String>,
    #[serde(alias = "pr-command")]
    pr_command: Option<String>,
    #[serde(alias = "review-loop")]
    review_loop: Option<String>,
    #[serde(alias = "merge-loop")]
    merge_loop: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDockConfig {
    visible: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawSessionConfig {
    auto_resume_agents: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawUpdateConfig {
    installed_binary_path: Option<String>,
}

fn parse_app_config_from_str(contents: &str) -> Result<AppConfig, ConfigError> {
    let raw: RawAppConfig = toml::from_str(contents).map_err(ConfigError::Toml)?;

    let config = AppConfig {
        appearance: parse_appearance_config(raw.appearance),
        sidebar: parse_sidebar_config(raw.sidebar),
        terminal: parse_terminal_config(raw.terminal),
        mise: parse_mise_config(raw.mise),
        tmux: parse_tmux_config(raw.tmux),
        http: parse_http_config(raw.http),
        history: parse_history_config(raw.history),
        http_control: parse_http_control_config(raw.http_control),
        tasks: parse_tasks_config(raw.tasks),
        dock: parse_dock_config(raw.dock),
        session: parse_session_config(raw.session),
        update: parse_update_config(raw.update),
        editor: parse_editor_config(raw.editor),
        browser: parse_browser_config(raw.browser),
        loop_runner: parse_loop_runner_config(raw.loop_runner),
        hosts: parse_hosts_config(raw.hosts),
    };
    if let Err(errors) = config.history.validate() {
        return Err(ConfigError::Invalid {
            errors,
            config: Box::new(config),
        });
    }
    Ok(config)
}

fn parse_dock_config(raw: Option<RawDockConfig>) -> DockConfig {
    DockConfig {
        visible: raw.and_then(|dock| dock.visible).unwrap_or(false),
    }
}

fn parse_session_config(raw: Option<RawSessionConfig>) -> SessionConfig {
    SessionConfig {
        auto_resume_agents: raw
            .and_then(|session| session.auto_resume_agents)
            .unwrap_or(false),
    }
}

fn parse_update_config(raw: Option<RawUpdateConfig>) -> UpdateConfig {
    let installed_binary_path = raw
        .and_then(|update| update.installed_binary_path)
        .and_then(|value| {
            let trimmed = value.trim();
            let path = PathBuf::from(trimmed);
            if trimmed.is_empty() || !path.is_absolute() {
                eprintln!(
                    "taarof: invalid update.installed_binary_path; expected a non-empty absolute path, using default search order"
                );
                None
            } else {
                Some(path)
            }
        });
    UpdateConfig {
        installed_binary_path,
    }
}

fn parse_appearance_config(raw: RawAppearanceConfig) -> AppearanceConfig {
    AppearanceConfig {
        base: raw.base.as_deref().map(normalize_color),
        mantle: raw.mantle.as_deref().map(normalize_color),
        crust: raw.crust.as_deref().map(normalize_color),
        surface: raw.surface.as_deref().map(normalize_color),
        surface_raised: raw.surface_raised.as_deref().map(normalize_color),
        border: raw.border.as_deref().map(normalize_color),
        text: raw.text.as_deref().map(normalize_color),
        subtext: raw.subtext.as_deref().map(normalize_color),
        muted: raw.muted.as_deref().map(normalize_color),
        title: raw.title.as_deref().map(normalize_color),
        soft_text: raw.soft_text.as_deref().map(normalize_color),
        warm_muted: raw.warm_muted.as_deref().map(normalize_color),
        accent: raw.accent.as_deref().map(normalize_color),
        accent_hover: raw.accent_hover.as_deref().map(normalize_color),
        running: raw.running.as_deref().map(normalize_color),
        activity: raw.activity.as_deref().map(normalize_color),
        waiting: raw.waiting.as_deref().map(normalize_color),
        ports: raw.ports.as_deref().map(normalize_color),
        error: raw.error.as_deref().map(normalize_color),
    }
}

fn parse_sidebar_config(raw: RawSidebarConfig) -> SidebarConfig {
    SidebarConfig {
        position: parse_sidebar_position(raw.position.as_deref()),
        compact: raw.compact.unwrap_or(false),
        theme: SidebarTheme {
            background: raw.background.as_deref().map(normalize_color),
            surface: raw.surface.as_deref().map(normalize_color),
            accent: raw.accent.as_deref().map(normalize_color),
            text: raw.text.as_deref().map(normalize_color),
            muted: raw.muted.as_deref().map(normalize_color),
        },
        workspace_icons: WorkspaceIconConfig {
            default: raw
                .workspace_icons
                .default
                .as_deref()
                .and_then(normalize_icon),
            worktree: raw
                .workspace_icons
                .worktree
                .as_deref()
                .and_then(normalize_icon),
            by_name: normalize_icon_map(raw.workspace_icons.by_name),
            by_repo: normalize_icon_map(raw.workspace_icons.by_repo),
        },
    }
}

fn parse_terminal_config(raw: Option<RawTerminalConfig>) -> TerminalConfig {
    match raw {
        Some(raw_terminal) => TerminalConfig {
            copy_recent_lines: parse_copy_recent_lines(raw_terminal.copy_recent_lines),
            clipboard_history_size: parse_clipboard_history_size(
                raw_terminal.clipboard_history_size,
            ),
        },
        None => TerminalConfig::default(),
    }
}

fn parse_mise_config(raw: Option<RawMiseConfig>) -> MiseConfig {
    match raw {
        Some(raw_mise) => MiseConfig {
            include_global: raw_mise.include_global.unwrap_or(false),
        },
        None => MiseConfig::default(),
    }
}

fn parse_tmux_config(raw: Option<RawTmuxConfig>) -> TmuxConfig {
    match raw {
        Some(raw_tmux) => {
            let close_behavior = match raw_tmux.close_behavior.as_deref() {
                Some("detach") => TmuxCloseBehavior::Detach,
                _ => TmuxCloseBehavior::Close,
            };
            // Anything but an explicit "plain" inherits, so a typo degrades to
            // today's behavior instead of restyling the user's tmux. Config
            // validation reports the typo separately.
            let session_style = match raw_tmux.session_style.as_deref() {
                Some("plain") => crate::tmux::TmuxSessionStyle::Plain,
                _ => crate::tmux::TmuxSessionStyle::Inherit,
            };
            TmuxConfig {
                enabled: raw_tmux.enabled.unwrap_or(false),
                session_prefix: raw_tmux.session_prefix.unwrap_or_else(|| "taarof".into()),
                close_behavior,
                session_style,
            }
        }
        None => TmuxConfig::default(),
    }
}

fn parse_http_config(raw: Option<RawHttpConfig>) -> HttpConfig {
    match raw {
        Some(raw_http) => HttpConfig {
            enabled: raw_http.enabled.unwrap_or(false),
            port: raw_http.port.unwrap_or(7800),
            bind_address: raw_http.bind_address.unwrap_or_else(|| "127.0.0.1".into()),
            unsafe_allow_non_loopback: raw_http.unsafe_allow_non_loopback.unwrap_or(false),
        },
        None => HttpConfig::default(),
    }
}

fn parse_history_config(raw: Option<RawHistoryConfig>) -> HistoryConfig {
    let defaults = HistoryConfig::default();
    match raw {
        Some(raw) => HistoryConfig {
            enabled: raw.enabled.unwrap_or(defaults.enabled),
            max_age_days: raw.max_age_days.unwrap_or(defaults.max_age_days),
            max_records: raw.max_records.unwrap_or(defaults.max_records),
            max_bytes: raw.max_bytes.unwrap_or(defaults.max_bytes),
            maintenance_interval_minutes: raw
                .maintenance_interval_minutes
                .unwrap_or(defaults.maintenance_interval_minutes),
            queue_capacity: raw.queue_capacity.unwrap_or(defaults.queue_capacity),
            record_events: raw.record_events.unwrap_or(defaults.record_events),
            record_diagnostics: raw
                .record_diagnostics
                .unwrap_or(defaults.record_diagnostics),
            record_work: raw.record_work.unwrap_or(defaults.record_work),
            config_error: None,
        },
        None => defaults,
    }
}

fn parse_http_control_config(raw: Option<RawHttpControlConfig>) -> HttpControlConfig {
    match raw {
        Some(raw_http_control) => HttpControlConfig {
            enabled: raw_http_control.enabled.unwrap_or(false),
        },
        None => HttpControlConfig::default(),
    }
}

fn parse_tasks_config(raw: Option<RawTasksConfig>) -> TasksConfig {
    match raw {
        Some(raw_tasks) => TasksConfig {
            enabled: raw_tasks.enabled.unwrap_or(false),
            pull_requests: raw_tasks.pull_requests.unwrap_or(false),
            default_view: parse_task_panel_default_view(raw_tasks.default_view.as_deref()),
        },
        None => TasksConfig::default(),
    }
}

fn parse_task_panel_default_view(value: Option<&str>) -> TaskPanelDefaultView {
    match value.unwrap_or_default().trim() {
        "pull_requests" | "pull-requests" | "prs" | "PRs" => TaskPanelDefaultView::PullRequests,
        _ => TaskPanelDefaultView::Tasks,
    }
}

fn parse_loop_runner_config(raw: Option<RawLoopRunnerConfig>) -> LoopRunnerConfig {
    let defaults = LoopRunnerConfig::default();
    match raw {
        Some(raw_loop_runner) => LoopRunnerConfig {
            enabled: raw_loop_runner.enabled.unwrap_or(defaults.enabled),
            dev_command: non_empty_or(raw_loop_runner.dev_command, defaults.dev_command),
            pr_command: non_empty_or(raw_loop_runner.pr_command, defaults.pr_command),
            review_loop: non_empty_or(raw_loop_runner.review_loop, defaults.review_loop),
            merge_loop: non_empty_or(raw_loop_runner.merge_loop, defaults.merge_loop),
        },
        None => defaults,
    }
}

/// Use `value` when it has non-whitespace content, otherwise fall back to `default`.
fn non_empty_or(value: Option<String>, default: String) -> String {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or(default)
}

fn parse_editor_config(raw: Option<RawEditorConfig>) -> EditorConfig {
    match raw {
        Some(raw_editor) => EditorConfig {
            open_command: parse_open_command(raw_editor.open_command),
            click_action: parse_click_action(raw_editor.click_action.as_deref()),
        },
        None => EditorConfig::default(),
    }
}

/// Parse the `[browser]` section. A blank/whitespace `open_command` means
/// "use the system default browser", same as leaving the section out.
fn parse_browser_config(raw: Option<RawBrowserConfig>) -> BrowserConfig {
    BrowserConfig {
        open_command: raw
            .and_then(|raw_browser| raw_browser.open_command)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    }
}

/// Parse the `[editor] click_action` field. Unknown/blank values default to the
/// in-app peek overlay (the feature's intended behavior).
fn parse_click_action(value: Option<&str>) -> ClickAction {
    match value.map(str::trim) {
        Some("editor") => ClickAction::Editor,
        _ => ClickAction::Peek,
    }
}

fn parse_hosts_config(
    raw: Option<BTreeMap<String, RawHostConfig>>,
) -> Vec<crate::host::HostConfig> {
    raw.unwrap_or_default()
        .into_iter()
        .map(|(name, raw_host)| {
            let defaults = crate::host::HostConfig::default();
            crate::host::HostConfig {
                name,
                ssh_target: raw_host.address,
                max_sessions: raw_host.max_sessions.unwrap_or(defaults.max_sessions),
                warn_cpu_percent: raw_host
                    .warn_cpu_percent
                    .unwrap_or(defaults.warn_cpu_percent),
                warn_memory_percent: raw_host
                    .warn_memory_percent
                    .unwrap_or(defaults.warn_memory_percent),
                idle_detach_minutes: raw_host
                    .idle_detach_minutes
                    .unwrap_or(defaults.idle_detach_minutes),
                tmux_backed: raw_host.tmux_backed.unwrap_or(true),
            }
        })
        .collect()
}

fn parse_sidebar_position(value: Option<&str>) -> SidebarPosition {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => SidebarPosition::Left,
        Some(value) if value.eq_ignore_ascii_case("left") => SidebarPosition::Left,
        Some(value) if value.eq_ignore_ascii_case("right") => SidebarPosition::Right,
        Some(value) => {
            eprintln!("taarof: invalid sidebar.position \"{value}\", using left");
            SidebarPosition::Left
        }
    }
}

fn parse_copy_recent_lines(value: Option<u32>) -> u32 {
    let default = TerminalConfig::default().copy_recent_lines;
    match value {
        None => default,
        Some(0) => {
            eprintln!("taarof: invalid terminal.copy_recent_lines \"0\", using {default}");
            default
        }
        Some(value) if value > crate::terminal::MAX_CAPTURE_SCROLLBACK_LINES => {
            eprintln!(
                "taarof: invalid terminal.copy_recent_lines \"{value}\", max is {}, using {default}",
                crate::terminal::MAX_CAPTURE_SCROLLBACK_LINES
            );
            default
        }
        Some(value) => value,
    }
}

/// Maximum number of entries the in-memory clipboard history ring may hold.
/// Values above this are rejected (fall back to the default) to bound memory —
/// terminal copies can be large.
const MAX_CLIPBOARD_HISTORY_SIZE: u32 = 1000;

fn parse_clipboard_history_size(value: Option<u32>) -> u32 {
    let default = TerminalConfig::default().clipboard_history_size;
    match value {
        None => default,
        Some(0) => {
            eprintln!("taarof: invalid terminal.clipboard_history_size \"0\", using {default}");
            default
        }
        Some(value) if value > MAX_CLIPBOARD_HISTORY_SIZE => {
            eprintln!(
                "taarof: invalid terminal.clipboard_history_size \"{value}\", max is {MAX_CLIPBOARD_HISTORY_SIZE}, using {default}"
            );
            default
        }
        Some(value) => value,
    }
}

fn parse_open_command(value: Option<String>) -> String {
    value
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| EditorConfig::default().open_command)
}

fn normalize_icon(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn normalize_icon_map(source: BTreeMap<String, String>) -> BTreeMap<String, String> {
    source
        .into_iter()
        .filter_map(|(key, value)| {
            normalize_icon(&value).map(|value| (key.trim().to_string(), value))
        })
        .collect()
}

fn find_icon_override(map: &BTreeMap<String, String>, key: &str) -> Option<String> {
    map.iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .map(|(_, icon)| icon.clone())
}

fn path_basename(path: &str) -> Option<&str> {
    Path::new(path).file_name()?.to_str()
}

fn derive_workspace_icon(workspace_name: &str, repo_root: Option<&str>) -> String {
    repo_root
        .and_then(path_basename)
        .unwrap_or(workspace_name)
        .chars()
        .find(|ch| ch.is_alphanumeric())
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "•".to_string())
}

/// Parsed Ghostty configuration with all settings relevant to VTE terminals.
#[derive(Debug, Clone)]
pub struct GhosttyConfig {
    pub font_family: String,
    pub font_size: f64,
    pub background: String,
    pub foreground: String,
    pub cursor_color: String,
    pub selection_bg: String,
    pub selection_fg: String,
    pub palette: [String; 16],
    pub cursor_style: String,
    pub cursor_blink: bool,
    pub copy_on_select: bool,
    pub scrollback_lines: i64,
    pub shell_command: Option<String>,
    pub padding_x: i32,
    pub padding_y: i32,
}

impl Default for GhosttyConfig {
    fn default() -> Self {
        Self {
            font_family: "JetBrainsMono Nerd Font".into(),
            font_size: 12.0,
            background: "#14110c".into(),
            foreground: "#efe7d6".into(),
            cursor_color: "#efe7d6".into(),
            selection_bg: "#463726".into(),
            selection_fg: "#efe7d6".into(),
            palette: [
                "#34291a", "#d6796b", "#7bbf7a", "#d9b34a", "#e0a43a", "#e0a43a", "#6bb0b8",
                "#d8cdb6", "#463726", "#d6796b", "#7bbf7a", "#d9b34a", "#e0a43a", "#e0a43a",
                "#6bb0b8", "#c3b9a7",
            ]
            .map(String::from),
            cursor_style: "block".into(),
            cursor_blink: false,
            // Selection-first model: auto-copy any selection to the clipboard by
            // default. Override with `copy-on-select = false` in ghostty config.
            copy_on_select: true,
            scrollback_lines: 10000,
            shell_command: None,
            padding_x: 0,
            padding_y: 0,
        }
    }
}

impl GhosttyConfig {
    /// Load from ~/.config/ghostty/config, following `config-file` imports.
    pub fn load() -> Self {
        Self::load_from_path(&default_ghostty_config_path())
    }

    fn load_from_path(config_path: &Path) -> Self {
        let mut cfg = GhosttyConfig::default();

        if config_path.exists() {
            let entries = parse_file(config_path);
            cfg.apply(&entries);
        }

        cfg
    }

    fn apply(&mut self, entries: &[(String, String)]) {
        for (key, val) in entries {
            match key.as_str() {
                "font-family" => self.font_family = unquote(val),
                "font-size" => {
                    if let Ok(s) = val.parse::<f64>() {
                        self.font_size = s;
                    }
                }
                "background" => self.background = normalize_color(val),
                "foreground" => self.foreground = normalize_color(val),
                "cursor-color" => self.cursor_color = normalize_color(val),
                "selection-background" => self.selection_bg = normalize_color(val),
                "selection-foreground" => self.selection_fg = normalize_color(val),
                "palette" => {
                    // format: "N=#RRGGBB" or "N=name"
                    if let Some((idx_str, color)) = val.split_once('=') {
                        if let Ok(idx) = idx_str.trim().parse::<usize>() {
                            if idx < 16 {
                                self.palette[idx] = normalize_color(color.trim());
                            }
                        }
                    }
                }
                "cursor-style" => self.cursor_style = unquote(val),
                "cursor-style-blink" => self.cursor_blink = val.eq_ignore_ascii_case("true"),
                "copy-on-select" => self.copy_on_select = val.eq_ignore_ascii_case("true"),
                "command" => self.shell_command = Some(unquote(val)),
                "window-padding-x" => {
                    if let Ok(p) = val.parse::<i32>() {
                        self.padding_x = p;
                    }
                }
                "window-padding-y" => {
                    if let Ok(p) = val.parse::<i32>() {
                        self.padding_y = p;
                    }
                }
                _ => {}
            }
        }
    }

    /// Build a pango font description string.
    pub fn font_description(&self) -> String {
        format!("{} {}", self.font_family, self.font_size)
    }
}

/// Parse a Ghostty config file, recursively following `config-file` imports.
fn parse_file(path: &Path) -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let parent_dir = path.parent().unwrap_or(Path::new("."));
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim().to_string();
            let val = val.trim().to_string();

            if key == "config-file" {
                // Handle optional imports: config-file = ?path
                let import_path = if let Some(stripped) = val.strip_prefix('?') {
                    stripped.trim()
                } else {
                    val.as_str()
                };
                let import_path = unquote(import_path);
                let resolved = resolve_path(&import_path, parent_dir);
                if resolved.exists() {
                    entries.extend(parse_file(&resolved));
                }
            } else {
                entries.push((key, val));
            }
        }
    }

    entries
}

/// Expand ~ and resolve relative paths against a base directory.
fn resolve_path(path: &str, base: &Path) -> PathBuf {
    let path = if path.starts_with("~/") || path.starts_with("~\"") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    };
    let path = unquote(&path);
    let p = PathBuf::from(&path);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

/// Strip surrounding quotes from a string.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Ensure color string has a # prefix.
fn normalize_color(s: &str) -> String {
    let s = unquote(s);
    let s = s.trim();
    if s.starts_with('#') {
        return s.to_string();
    }
    let is_hex = matches!(s.len(), 3 | 6 | 8) && s.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex {
        format!("#{s}")
    } else {
        s.to_string()
    }
}

// ── Headless config CLI: config-paths / config-default / config-validate ──
//
// These subcommands are dispatched from `main.rs` *before* GTK is initialized.
// They must never touch GTK, the socket, or any live runtime state; they only
// read/parse config files with the real loaders and report findings.

/// Severity of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// A single validation finding tied to a config file.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Which file the finding applies to (e.g. "config.toml").
    pub file: String,
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    pub fn error(file: &str, message: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            severity: Severity::Error,
            message: message.into(),
        }
    }

    pub fn warning(file: &str, message: impl Into<String>) -> Self {
        Self {
            file: file.to_string(),
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

/// A colour override is only meaningful if it is a `#rgb`/`#rrggbb`/`#rrggbbaa`
/// hex value or a plausible CSS named colour. `normalize_color` already turns a
/// bare hex string into `#…`, so anything that reaches here without a leading
/// `#` and is not a bare word is suspicious.
fn color_looks_valid(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    // Named colours: a single ASCII-alphabetic token (e.g. "red", "cornsilk").
    value.chars().all(|c| c.is_ascii_alphabetic())
}

/// Validate the app `config.toml` at `path`. A missing file is not an error
/// (defaults are used); the returned Vec is empty in that case.
pub fn validate_app_config_file(path: &Path) -> Vec<Finding> {
    const FILE: &str = "config.toml";
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
    validate_app_config_str(&contents)
}

/// Validate the raw contents of an app `config.toml`.
///
/// First runs the *real* strict `toml` parse (surfacing type errors the lenient
/// loader would otherwise swallow via `serde(default)`), then does a raw
/// `toml::Value` pass to catch value-level mistakes (bad `close_behavior`,
/// malformed hosts, invalid colours) that the lenient loader silently
/// normalizes to a default.
pub fn validate_app_config_str(contents: &str) -> Vec<Finding> {
    const FILE: &str = "config.toml";
    let mut findings = Vec::new();

    // 1. Strict typed parse against the real Raw* structs. This catches wrong
    //    value *types* (e.g. `port = "nope"`) that the lenient file loader would
    //    swallow and replace with a default.
    let raw = match toml::from_str::<RawAppConfig>(contents) {
        Ok(raw) => raw,
        Err(err) => {
            findings.push(Finding::error(FILE, format!("parse error: {err}")));
            // A hard parse error means the value-level checks below cannot run
            // meaningfully; return what we have.
            return findings;
        }
    };
    if let Err(errors) = parse_history_config(raw.history).validate() {
        findings.extend(errors.into_iter().map(|error| Finding::error(FILE, error)));
    }

    // 2. Raw value pass for value-level rules the lenient loader normalizes away.
    let value: toml::Value = match toml::from_str(contents) {
        Ok(value) => value,
        Err(err) => {
            findings.push(Finding::error(FILE, format!("parse error: {err}")));
            return findings;
        }
    };
    let Some(table) = value.as_table() else {
        return findings;
    };

    // tmux.close_behavior must be "close" or "detach".
    if let Some(tmux) = table.get("tmux").and_then(|v| v.as_table()) {
        if let Some(cb) = tmux.get("close_behavior") {
            match cb.as_str() {
                Some("close") | Some("detach") => {}
                Some(other) => findings.push(Finding::error(
                    FILE,
                    format!(
                        "tmux.close_behavior \"{other}\" is invalid (expected \"close\" or \"detach\")"
                    ),
                )),
                None => findings.push(Finding::error(
                    FILE,
                    "tmux.close_behavior must be a string (\"close\" or \"detach\")",
                )),
            }
        }
        // tmux.session_style must be "inherit" or "plain".
        if let Some(style) = tmux.get("session_style") {
            match style.as_str() {
                Some("inherit") | Some("plain") => {}
                Some(other) => findings.push(Finding::error(
                    FILE,
                    format!(
                        "tmux.session_style \"{other}\" is invalid (expected \"inherit\" or \"plain\")"
                    ),
                )),
                None => findings.push(Finding::error(
                    FILE,
                    "tmux.session_style must be a string (\"inherit\" or \"plain\")",
                )),
            }
        }
    }

    // hosts.<name>: address, when present, must be a non-empty string; the
    // numeric knobs must be integers.
    if let Some(hosts) = table.get("hosts").and_then(|v| v.as_table()) {
        for (name, host) in hosts {
            let Some(host) = host.as_table() else {
                findings.push(Finding::error(
                    FILE,
                    format!("hosts.{name} must be a table"),
                ));
                continue;
            };
            match host.get("address") {
                None => findings.push(Finding::warning(
                    FILE,
                    format!("hosts.{name} has no address (treated as a local-only entry)"),
                )),
                Some(addr) => match addr.as_str() {
                    Some(s) if s.trim().is_empty() => findings.push(Finding::error(
                        FILE,
                        format!("hosts.{name}.address is empty"),
                    )),
                    Some(_) => {}
                    None => findings.push(Finding::error(
                        FILE,
                        format!("hosts.{name}.address must be a string"),
                    )),
                },
            }
            for key in [
                "max_sessions",
                "warn_cpu_percent",
                "warn_memory_percent",
                "idle_detach_minutes",
            ] {
                if let Some(v) = host.get(key) {
                    if v.as_integer().is_none() {
                        findings.push(Finding::error(
                            FILE,
                            format!("hosts.{name}.{key} must be an integer"),
                        ));
                    }
                }
            }
            if let Some(v) = host.get("tmux_backed") {
                if v.as_bool().is_none() {
                    findings.push(Finding::error(
                        FILE,
                        format!("hosts.{name}.tmux_backed must be a boolean"),
                    ));
                }
            }
        }
    }

    // Full-app appearance roles participate in rgba() substitutions, so they
    // require exact six-digit RGB hex values rather than named/short colours.
    if let Some(appearance) = table.get("appearance").and_then(|v| v.as_table()) {
        for key in [
            "base",
            "mantle",
            "crust",
            "surface",
            "surface_raised",
            "surface-raised",
            "border",
            "text",
            "subtext",
            "muted",
            "title",
            "soft_text",
            "soft-text",
            "warm_muted",
            "warm-muted",
            "accent",
            "accent_hover",
            "accent-hover",
            "running",
            "activity",
            "waiting",
            "ports",
            "error",
        ] {
            if let Some(raw) = appearance.get(key) {
                match raw.as_str() {
                    Some(value) if is_six_digit_hex(&normalize_color(value)) => {}
                    Some(value) => findings.push(Finding::error(
                        FILE,
                        format!("appearance.{key} \"{value}\" must be a six-digit RGB hex colour"),
                    )),
                    None => findings.push(Finding::error(
                        FILE,
                        format!("appearance.{key} must be a string colour"),
                    )),
                }
            }
        }
    }

    // sidebar colour overrides must be hex or a plausible named colour.
    if let Some(sidebar) = table.get("sidebar").and_then(|v| v.as_table()) {
        for key in ["background", "surface", "accent", "text", "muted"] {
            if let Some(raw) = sidebar.get(key) {
                match raw.as_str() {
                    Some(s) => {
                        let normalized = normalize_color(s);
                        if !color_looks_valid(&normalized) {
                            findings.push(Finding::error(
                                FILE,
                                format!("sidebar.{key} \"{s}\" is not a valid colour"),
                            ));
                        }
                    }
                    None => findings.push(Finding::error(
                        FILE,
                        format!("sidebar.{key} must be a string colour"),
                    )),
                }
            }
        }
    }

    findings
}

/// Produce a commented starter `config.toml`. Values are derived from the real
/// default structs so this stays the single source of truth for defaults.
pub fn generate_default_config_toml() -> String {
    let tmux = TmuxConfig::default();
    let http = HttpConfig::default();
    let history = HistoryConfig::default();
    let terminal = TerminalConfig::default();
    let editor = EditorConfig::default();
    let session = SessionConfig::default();
    let loop_runner = LoopRunnerConfig::default();
    let host = crate::host::HostConfig::default();

    let tmux_close = match tmux.close_behavior {
        TmuxCloseBehavior::Close => "close",
        TmuxCloseBehavior::Detach => "detach",
    };
    let tmux_session_style = match tmux.session_style {
        crate::tmux::TmuxSessionStyle::Inherit => "inherit",
        crate::tmux::TmuxSessionStyle::Plain => "plain",
    };

    format!(
        r##"# taarof configuration (~/.config/taarof/config.toml)
#
# Every value below is a default; uncomment and edit to override. Generated by
# `taarof-app config-default`. Keybindings live in a separate keybindings.toml.

[appearance]
# Full-app Majlis colour roles. Values must be six-digit RGB hex colours.
# Terminal-pane colours still come from ~/.config/ghostty/config.
# base = "#14110c"
# mantle = "#100d09"
# crust = "#0b0906"
# surface = "#241d13"
# surface_raised = "#34291a"
# border = "#463726"
# text = "#efe7d6"
# subtext = "#c3b9a7"
# muted = "#7c715e"
# title = "#f6f0e4"
# soft_text = "#d8cdb6"
# warm_muted = "#a08d6f"
# accent = "#e0a43a"
# accent_hover = "#f0be5e"
# running = "#7bbf7a"
# activity = "#6bb0b8"
# waiting = "#d9b34a"
# ports = "#e0913a"
# error = "#d6796b"

[sidebar]
# position = "left"          # "left" or "right"
# compact = false            # true = icon-and-status rail
# Optional sidebar-only overrides (hex or a CSS colour name):
# background = "#100d09"
# surface = "#241d13"
# accent = "#e0a43a"
# text = "#efe7d6"
# muted = "#7c715e"

[terminal]
# copy_recent_lines = {copy_recent_lines}   # lines captured by "copy recent output"
# clipboard_history_size = {clipboard_history_size}   # entries kept in the in-memory clipboard history ring

[editor]
# open_command = "{open_command}"
# click_action = "peek"   # "peek" = in-app viewer, "editor" = eject to open_command

[browser]
# open_command = "helium-browser {{url}}"   # unset = system default browser

[mise]
# include_global = false

[tmux]
# enabled = {tmux_enabled}
# session_prefix = "{session_prefix}"
# close_behavior = "{tmux_close}"
#   "close"  = closing a tmux-backed pane/tab KILLS its tmux session (taarof
#              confirms before this destructive step and offers Detach instead).
#   "detach" = closing leaves the session running; reattach later from the
#              command palette. Nothing is lost when you close the pane.
# session_style = "{tmux_session_style}"
#   "inherit" = touch no tmux options; sessions look exactly like your tmux.
#   "plain"   = on sessions taarof creates, set status off and mouse on
#               (session-scoped only; your ~/.tmux.conf and global options are
#               never modified, and sessions taarof did not create are left
#               alone). See docs/tmux-integration.md for the tmux.conf recipe.

[http]
# enabled = {http_enabled}
# port = {http_port}
# bind_address = "{bind_address}"
# unsafe_allow_non_loopback = {http_unsafe}   # keep false unless you understand the risk

[history]
# enabled = {history_enabled}
# max_age_days = {history_max_age_days}
# max_records = {history_max_records}
# max_bytes = {history_max_bytes}   # soft cap; 8 MiB..=64 GiB, or 0 when another bound is set
# maintenance_interval_minutes = {history_maintenance_interval_minutes}
# queue_capacity = {history_queue_capacity}
# record_events = {history_record_events}
# record_diagnostics = {history_record_diagnostics}
# record_work = {history_record_work}

[update]
# installed_binary_path = "/absolute/path/to/taarof-app"
# Omit to search ~/.local/bin, /usr/local/bin, then /usr/bin.

[http_control]
# enabled = false            # allow the local web client to write to panes

[tasks]
# enabled = false            # show the .plan/tasks.json task panel
# pull_requests = false
# default_view = "tasks"     # "tasks" or "pull-requests"

[dock]
# visible = false            # show the right-hand Session/Agents/Tasks dock at startup

[session]
# auto_resume_agents = {auto_resume_agents}  # false = offer resume without running it

[loop_runner]
# enabled = {loop_enabled}
# dev_command = "{dev_command}"
# pr_command = "{pr_command}"
# review_loop = "{review_loop}"
# merge_loop = "{merge_loop}"

# Remote hosts for tmux-backed sessions. Repeat the block per host.
# [hosts.example]
# address = "user@example.com"       # ssh target; omit for a local-only entry
# max_sessions = {max_sessions}
# warn_cpu_percent = {warn_cpu}
# warn_memory_percent = {warn_mem}
# idle_detach_minutes = {idle_detach}
# tmux_backed = {tmux_backed}         # new tabs/splits on this host inherit tmux
#                                     # backing (see docs/tmux-integration.md)
"##,
        copy_recent_lines = terminal.copy_recent_lines,
        clipboard_history_size = terminal.clipboard_history_size,
        open_command = editor.open_command,
        tmux_enabled = tmux.enabled,
        session_prefix = tmux.session_prefix,
        tmux_close = tmux_close,
        tmux_session_style = tmux_session_style,
        http_enabled = http.enabled,
        http_port = http.port,
        bind_address = http.bind_address,
        http_unsafe = http.unsafe_allow_non_loopback,
        history_enabled = history.enabled,
        history_max_age_days = history.max_age_days,
        history_max_records = history.max_records,
        history_max_bytes = history.max_bytes,
        history_maintenance_interval_minutes = history.maintenance_interval_minutes,
        history_queue_capacity = history.queue_capacity,
        history_record_events = history.record_events,
        history_record_diagnostics = history.record_diagnostics,
        history_record_work = history.record_work,
        loop_enabled = loop_runner.enabled,
        auto_resume_agents = session.auto_resume_agents,
        dev_command = loop_runner.dev_command,
        pr_command = loop_runner.pr_command,
        review_loop = loop_runner.review_loop,
        merge_loop = loop_runner.merge_loop,
        max_sessions = host.max_sessions,
        warn_cpu = host.warn_cpu_percent,
        warn_mem = host.warn_memory_percent,
        idle_detach = host.idle_detach_minutes,
        tmux_backed = host.tmux_backed,
    )
}

/// Entry point for the headless config subcommands, dispatched from `main.rs`.
///
/// Returns `Some(exit_code)` when `args` selected a config subcommand (the
/// caller should exit with that code), or `None` when the first argument is not
/// a config subcommand (the caller should launch the GUI as usual).
///
/// `args` is the process argv *without* argv[0].
pub fn run_config_cli(args: &[String]) -> Option<i32> {
    let (cmd, rest) = args.split_first()?;
    match cmd.as_str() {
        "config-paths" => Some(run_config_paths(rest)),
        "config-default" => Some(run_config_default(rest)),
        "config-validate" => Some(run_config_validate(rest)),
        _ => None,
    }
}

fn run_config_paths(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let config_path = app_config_path();
    let keybindings_path = crate::keybindings::keybindings_config_path();
    if json {
        // Hand-rolled to avoid pulling serde_json paths into this cold code and
        // to keep escaping obvious for two known-shape string fields.
        println!(
            "{{\"config\":{},\"keybindings\":{}}}",
            json_string(&config_path.display().to_string()),
            json_string(&keybindings_path.display().to_string()),
        );
    } else {
        println!("config: {}", config_path.display());
        println!("keybindings: {}", keybindings_path.display());
    }
    0
}

fn run_config_default(_args: &[String]) -> i32 {
    print!("{}", generate_default_config_toml());
    0
}

fn run_config_validate(args: &[String]) -> i32 {
    let mut config_path: Option<PathBuf> = None;
    let mut keybindings_path: Option<PathBuf> = None;
    let mut json = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--config" => {
                let Some(value) = iter.next() else {
                    eprintln!("config-validate: --config requires a path");
                    return 2;
                };
                config_path = Some(PathBuf::from(value));
            }
            "--keybindings" => {
                let Some(value) = iter.next() else {
                    eprintln!("config-validate: --keybindings requires a path");
                    return 2;
                };
                keybindings_path = Some(PathBuf::from(value));
            }
            other => {
                eprintln!("config-validate: unrecognized argument \"{other}\"");
                return 2;
            }
        }
    }

    let config_path = config_path.unwrap_or_else(app_config_path);
    let keybindings_path =
        keybindings_path.unwrap_or_else(crate::keybindings::keybindings_config_path);

    let mut findings = validate_app_config_file(&config_path);
    findings.extend(crate::keybindings::validate_keybindings_file(
        &keybindings_path,
    ));

    let config_missing = !config_path.exists();
    let keybindings_missing = !keybindings_path.exists();

    let has_error = findings.iter().any(|f| f.severity == Severity::Error);

    if json {
        print_findings_json(&findings, !has_error);
    } else {
        if config_missing {
            println!(
                "config.toml: not found at {} (using defaults)",
                config_path.display()
            );
        }
        if keybindings_missing {
            println!(
                "keybindings.toml: not found at {} (using defaults)",
                keybindings_path.display()
            );
        }
        if findings.is_empty() {
            println!("config: valid");
        } else {
            for finding in &findings {
                println!(
                    "{}: {}: {}",
                    finding.file,
                    finding.severity.as_str(),
                    finding.message
                );
            }
        }
    }

    if has_error {
        1
    } else {
        0
    }
}

fn print_findings_json(findings: &[Finding], ok: bool) {
    let mut out = String::new();
    out.push_str("{\"ok\":");
    out.push_str(if ok { "true" } else { "false" });
    out.push_str(",\"findings\":[");
    for (idx, finding) in findings.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str("{\"file\":");
        out.push_str(&json_string(&finding.file));
        out.push_str(",\"severity\":");
        out.push_str(&json_string(finding.severity.as_str()));
        out.push_str(",\"message\":");
        out.push_str(&json_string(&finding.message));
        out.push('}');
    }
    out.push_str("]}");
    println!("{out}");
}

/// Minimal JSON string encoder for the two-field shapes emitted above.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn new_test_dir() -> PathBuf {
        let id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("taarof-config-tests-{}-{}", std::process::id(), id));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    #[test]
    fn test_normalize_color_preserves_hex() {
        assert_eq!(normalize_color("#abcdef"), "#abcdef");
        assert_eq!(normalize_color("abcdef"), "#abcdef");
    }

    fn messages(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(|f| f.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn validate_accepts_a_valid_config() {
        let toml_str = r##"
[sidebar]
position = "right"
accent = "#89b4fa"

[tmux]
enabled = true
close_behavior = "detach"

[hosts.box]
address = "user@box.example.com"
max_sessions = 4
"##;
        let findings = validate_app_config_str(toml_str);
        assert!(
            findings.iter().all(|f| f.severity != Severity::Error),
            "valid config should produce no errors, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_flags_invalid_close_behavior() {
        let findings = validate_app_config_str("[tmux]\nclose_behavior = \"explode\"\n");
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("close_behavior")),
            "expected a close_behavior error, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_flags_malformed_host() {
        let findings = validate_app_config_str("[hosts.box]\naddress = \"\"\n");
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("address")),
            "expected an empty-address error, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_flags_bad_host_field_type() {
        let findings = validate_app_config_str(
            "[hosts.box]\naddress = \"user@box\"\nmax_sessions = \"lots\"\n",
        );
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("max_sessions")),
            "expected a max_sessions type error, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_flags_invalid_color() {
        let findings = validate_app_config_str("[sidebar]\naccent = \"#zzz\"\n");
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("accent")),
            "expected an invalid-colour error, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_flags_non_rgb_appearance_color() {
        let findings = validate_app_config_str("[appearance]\naccent = \"gold\"\n");
        assert!(
            findings.iter().any(|finding| {
                finding.severity == Severity::Error
                    && finding.message.contains("appearance.accent")
                    && finding.message.contains("six-digit RGB hex")
            }),
            "expected an appearance colour error, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn validate_missing_file_is_ok() {
        let dir = new_test_dir();
        let missing = dir.join("does-not-exist.toml");
        assert!(validate_app_config_file(&missing).is_empty());
    }

    #[test]
    fn generated_default_config_parses_and_validates_clean() {
        let toml_str = generate_default_config_toml();
        // Every line of substance is commented out, so it parses to defaults and
        // validates without findings.
        parse_app_config_from_str(&toml_str).expect("generated default should parse");
        let findings = validate_app_config_str(&toml_str);
        assert!(
            findings.is_empty(),
            "generated default should be clean, got: {}",
            messages(&findings)
        );
    }

    #[test]
    fn test_normalize_color_preserves_named_values() {
        assert_eq!(normalize_color("red"), "red");
        assert_eq!(normalize_color("\"cornsilk\""), "cornsilk");
    }

    #[test]
    fn test_command_is_unquoted() {
        let mut cfg = GhosttyConfig::default();
        cfg.apply(&[("command".into(), "\"tmux new\"".into())]);
        assert_eq!(cfg.shell_command.as_deref(), Some("tmux new"));
    }

    #[test]
    fn tasks_panel_is_opt_in_and_off_by_default() {
        // No [tasks] section -> feature stays disabled.
        let default_config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(
            !default_config.tasks.enabled,
            "task-tracking UI must default to off"
        );

        // Explicit opt-in flips it on.
        let enabled = parse_app_config_from_str(
            r##"
[tasks]
enabled = true
"##,
        )
        .expect("config should parse");
        assert!(
            enabled.tasks.enabled,
            "[tasks] enabled = true should opt in"
        );
    }

    #[test]
    fn right_dock_is_hidden_by_default_and_can_be_enabled() {
        let default_config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!default_config.dock.visible);

        let enabled = parse_app_config_from_str(
            r#"
[dock]
visible = true
"#,
        )
        .expect("dock config should parse");
        assert!(enabled.dock.visible);
    }

    #[test]
    fn agent_session_auto_resume_is_opt_in() {
        let default_config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!default_config.session.auto_resume_agents);

        let enabled = parse_app_config_from_str(
            r#"
[session]
auto_resume_agents = true
"#,
        )
        .expect("session config should parse");
        assert!(enabled.session.auto_resume_agents);
    }

    #[test]
    fn tasks_panel_pr_source_is_opt_in_and_can_default_to_prs() {
        let default_config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(
            !default_config.tasks.pull_requests,
            "PR lookup should stay disabled unless configured"
        );
        assert_eq!(
            default_config.tasks.default_view,
            TaskPanelDefaultView::Tasks
        );

        let enabled = parse_app_config_from_str(
            r##"
[tasks]
enabled = true
pull_requests = true
default_view = "pull_requests"
"##,
        )
        .expect("config should parse");
        assert!(enabled.tasks.pull_requests);
        assert_eq!(
            enabled.tasks.default_view,
            TaskPanelDefaultView::PullRequests
        );
    }

    #[test]
    fn loop_runner_is_off_by_default_with_documented_command_templates() {
        let default_config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(
            !default_config.loop_runner.enabled,
            "loop runner integration must default to off"
        );
        assert_eq!(
            default_config.loop_runner.dev_command,
            "loop-runner dev-loop --repo {repo} --issue {issue}"
        );
        assert_eq!(
            default_config.loop_runner.pr_command,
            "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}"
        );
        assert_eq!(default_config.loop_runner.review_loop, "pr-review-readonly");
        assert_eq!(default_config.loop_runner.merge_loop, "pr-review");
    }

    #[test]
    fn loop_runner_overrides_parse_and_blank_values_fall_back_to_defaults() {
        let config = parse_app_config_from_str(
            r##"
[loop_runner]
enabled = true
dev_command = "my-runner build --repo {repo} --issue {issue} --loop {loop}"
pr_command = "my-runner review --repo {repo} --pr {pr} --loop {loop}"
review_loop = "read-only"
merge_loop = ""
"##,
        )
        .expect("config should parse");

        assert!(config.loop_runner.enabled);
        assert_eq!(
            config.loop_runner.dev_command,
            "my-runner build --repo {repo} --issue {issue} --loop {loop}"
        );
        assert_eq!(
            config.loop_runner.pr_command,
            "my-runner review --repo {repo} --pr {pr} --loop {loop}"
        );
        assert_eq!(config.loop_runner.review_loop, "read-only");
        // Blank string falls back to the documented default rather than an empty loop name.
        assert_eq!(config.loop_runner.merge_loop, "pr-review");
    }

    #[test]
    fn appearance_config_rewrites_solid_and_alpha_theme_tokens() {
        let config = parse_app_config_from_str(
            r##"
[appearance]
base = "#102030"
accent = "123456"
running = "#abcdef"
"##,
        )
        .expect("appearance config should parse");

        assert_eq!(config.appearance.base.as_deref(), Some("#102030"));
        assert_eq!(config.appearance.accent.as_deref(), Some("#123456"));
        assert_eq!(config.appearance.running.as_deref(), Some("#abcdef"));

        let css = config.appearance.apply_to_css(
            ".base{color:#14110c}.accent{color:#e0a43a;background:rgba(224, 164, 58, 0.2)}",
        );
        assert!(css.contains("color:#102030"));
        assert!(css.contains("color:#123456"));
        assert!(css.contains("rgba(18, 52, 86, 0.2)"));
        assert!(!css.contains("#e0a43a"));

        let no_cascade = AppearanceConfig {
            base: Some("#e0a43a".into()),
            accent: Some("#123456".into()),
            ..AppearanceConfig::default()
        }
        .apply_to_css(".base{color:#14110c}.accent{color:#e0a43a}");
        assert_eq!(no_cascade, ".base{color:#e0a43a}.accent{color:#123456}");
    }

    #[test]
    fn partial_sidebar_override_inherits_full_app_appearance_roles() {
        let appearance = AppearanceConfig {
            mantle: Some("#111111".into()),
            surface: Some("#222222".into()),
            accent: Some("#abcdef".into()),
            text: Some("#eeeeee".into()),
            muted: Some("#777777".into()),
            base: Some("#050505".into()),
            ..AppearanceConfig::default()
        };
        let sidebar = SidebarTheme {
            accent: Some("#fedcba".into()),
            ..SidebarTheme::default()
        };
        let css = sidebar
            .override_css_with_appearance(&appearance)
            .expect("partial sidebar override should generate CSS");

        assert!(css.contains("background-color: #111111;"));
        assert!(css.contains("border-color: #222222;"));
        assert!(css.contains("color: #eeeeee;"));
        assert!(css.contains("color: #777777;"));
        assert!(css.contains("background-color: #fedcba;"));
        assert!(css.contains("color: #050505;"));
    }

    #[test]
    fn parses_sidebar_config_from_toml() {
        let config = parse_app_config_from_str(
            r##"
[sidebar]
position = "right"
compact = true
background = "101820"
surface = "#202938"
accent = "gold"
text = "f8fafc"
muted = "94a3b8"

[sidebar.workspace-icons]
default = "•"
worktree = "⑂"

[sidebar.workspace-icons.by-name]
infra = "🛠"

[sidebar.workspace-icons.by-repo]
taarof = "⌘"
"##,
        )
        .expect("config should parse");

        assert_eq!(config.sidebar.position, SidebarPosition::Right);
        assert!(config.sidebar.compact);
        assert_eq!(config.sidebar.theme.background.as_deref(), Some("#101820"));
        assert_eq!(config.sidebar.theme.surface.as_deref(), Some("#202938"));
        assert_eq!(config.sidebar.theme.accent.as_deref(), Some("gold"));
        assert_eq!(config.sidebar.theme.text.as_deref(), Some("#f8fafc"));
        assert_eq!(config.sidebar.theme.muted.as_deref(), Some("#94a3b8"));
        assert_eq!(config.sidebar.workspace_icons.default.as_deref(), Some("•"));
        assert_eq!(
            config.sidebar.workspace_icons.worktree.as_deref(),
            Some("⑂")
        );
        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .by_name
                .get("infra")
                .map(String::as_str),
            Some("🛠")
        );
        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .by_repo
                .get("taarof")
                .map(String::as_str),
            Some("⌘")
        );
    }

    #[test]
    fn majlis_defaults_are_used_for_sidebar_and_terminal_fallbacks() {
        let theme = SidebarTheme {
            accent: Some("gold".into()),
            ..SidebarTheme::default()
        };
        let css = theme
            .override_css_with_appearance(&AppearanceConfig::default())
            .expect("partial override should generate CSS");
        assert!(css.contains("background-color: #100d09;"));
        assert!(css.contains("border-color: #241d13;"));
        assert!(css.contains("color: #efe7d6;"));
        assert!(css.contains("color: #7c715e;"));
        assert!(css.contains(".sidebar-mark,"));
        assert!(css.contains(".sidebar-logo,"));

        let terminal = GhosttyConfig::default();
        assert_eq!(terminal.background, "#14110c");
        assert_eq!(terminal.foreground, "#efe7d6");
        assert_eq!(terminal.cursor_color, "#efe7d6");
        assert_eq!(terminal.selection_bg, "#463726");
        assert_eq!(terminal.palette[1], "#d6796b");
        assert_eq!(terminal.palette[2], "#7bbf7a");
        assert_eq!(terminal.palette[4], "#e0a43a");
        assert_eq!(terminal.palette[6], "#6bb0b8");
    }

    #[test]
    fn sidebar_theme_override_css_covers_hierarchy_state_selectors() {
        let theme = SidebarTheme {
            background: Some("#101820".into()),
            surface: Some("#202938".into()),
            accent: Some("#f59e0b".into()),
            text: Some("#f8fafc".into()),
            muted: Some("#94a3b8".into()),
        };

        let css = theme
            .override_css_with_appearance(&AppearanceConfig::default())
            .expect("css should be generated");

        assert!(css.contains(".tab-row:hover {"));
        assert!(css.contains("border-left-color: #f59e0b;"));
        assert!(css.contains(
            ".tab-row.active:hover {\n    background-color: #f59e0b;\n    border-color: #f59e0b;\n}"
        ));
        assert!(css.contains(
            ".workspace-header.active .ws-worktree-badge {\n    color: #f59e0b;\n    background-color: #101820;\n    border-color: #f59e0b;\n}"
        ));
    }

    #[test]
    fn test_parse_tmux_config_defaults() {
        let config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!config.tmux.enabled);
        assert_eq!(config.tmux.session_prefix, "taarof");
        assert_eq!(config.tmux.close_behavior, TmuxCloseBehavior::Close);
    }

    #[test]
    fn test_parse_tmux_config_enabled() {
        let config = parse_app_config_from_str(
            r#"
[tmux]
enabled = true
session_prefix = "myapp"
"#,
        )
        .expect("config should parse");
        assert!(config.tmux.enabled);
        assert_eq!(config.tmux.session_prefix, "myapp");
        assert_eq!(config.tmux.close_behavior, TmuxCloseBehavior::Close);
    }

    #[test]
    fn test_parse_tmux_close_behavior_detach() {
        let config = parse_app_config_from_str(
            r#"
[tmux]
enabled = true
close_behavior = "detach"
"#,
        )
        .expect("config should parse");
        assert_eq!(config.tmux.close_behavior, TmuxCloseBehavior::Detach);
    }

    #[test]
    fn test_parse_tmux_session_style_defaults_to_inherit() {
        let empty = parse_app_config_from_str("").expect("empty config should parse");
        assert_eq!(
            empty.tmux.session_style,
            crate::tmux::TmuxSessionStyle::Inherit
        );
        let tmux_table =
            parse_app_config_from_str("[tmux]\nenabled = true\n").expect("config should parse");
        assert_eq!(
            tmux_table.tmux.session_style,
            crate::tmux::TmuxSessionStyle::Inherit
        );
    }

    #[test]
    fn test_parse_tmux_session_style_plain() {
        let config = parse_app_config_from_str(
            r#"
[tmux]
enabled = true
session_style = "plain"
"#,
        )
        .expect("config should parse");
        assert_eq!(
            config.tmux.session_style,
            crate::tmux::TmuxSessionStyle::Plain
        );
    }

    #[test]
    fn test_validate_tmux_session_style_flags_invalid_value() {
        let findings = validate_app_config_str("[tmux]\nsession_style = \"fancy\"\n");
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.message.contains("session_style")),
            "expected a session_style error, got: {}",
            messages(&findings)
        );
        let valid = validate_app_config_str("[tmux]\nsession_style = \"plain\"\n");
        assert!(
            valid.iter().all(|f| f.severity != Severity::Error),
            "session_style = \"plain\" is valid, got: {}",
            messages(&valid)
        );
        let inherit = validate_app_config_str("[tmux]\nsession_style = \"inherit\"\n");
        assert!(
            inherit.iter().all(|f| f.severity != Severity::Error),
            "session_style = \"inherit\" is valid, got: {}",
            messages(&inherit)
        );
    }

    // ── EXAMPLE-75: tmux close/detach decision logic ───────────────────────────

    #[test]
    fn tmux_close_detach_behavior_close_warns_before_kill() {
        // close_behavior = close on a live tmux-backed pane must ask first.
        assert_eq!(
            tmux_close_consequence(TmuxCloseBehavior::Close, true),
            TmuxCloseConsequence::ConfirmKill,
        );
    }

    #[test]
    fn tmux_close_detach_behavior_detach_preserves_session() {
        // close_behavior = detach never kills, so no confirmation is needed.
        assert_eq!(
            tmux_close_consequence(TmuxCloseBehavior::Detach, true),
            TmuxCloseConsequence::DetachSurvives,
        );
    }

    #[test]
    fn tmux_close_detach_non_backed_pane_never_confirms() {
        // A plain (non-tmux) pane owns no session, so closing destroys nothing
        // regardless of the configured behavior.
        assert_eq!(
            tmux_close_consequence(TmuxCloseBehavior::Close, false),
            TmuxCloseConsequence::DetachSurvives,
        );
        assert_eq!(
            tmux_close_consequence(TmuxCloseBehavior::Detach, false),
            TmuxCloseConsequence::DetachSurvives,
        );
    }

    #[test]
    fn workspace_tmux_mode_explains_split_inheritance() {
        // Workspace opted in + tmux enabled → new tabs/splits inherit backing.
        assert!(tmux_inherits_backing(true, false, true));
        // A split from a tmux-backed pane inherits even if the workspace is off.
        assert!(tmux_inherits_backing(false, true, true));
        // Neither source → no inheritance.
        assert!(!tmux_inherits_backing(false, false, true));
        // Global tmux disabled → nothing inherits, even inside a tmux workspace.
        assert!(!tmux_inherits_backing(true, true, false));
    }

    #[test]
    fn tmux_close_detach_nesting_warning_predicate() {
        // No nesting when neither trigger fires.
        assert!(!tmux_nesting_warning(false, false));
        // taarof already inside tmux → local tmux-backed pane would nest.
        assert!(tmux_nesting_warning(true, false));
        // Remote tmux over SSH → remote shell may already be in tmux.
        assert!(tmux_nesting_warning(false, true));
        // Both triggers still warns.
        assert!(tmux_nesting_warning(true, true));
    }

    #[test]
    fn mise_include_global_defaults_to_false() {
        let config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!config.mise.include_global);
    }

    #[test]
    fn parses_mise_include_global_from_toml() {
        let config = parse_app_config_from_str(
            r#"
[mise]
include_global = true
"#,
        )
        .expect("config should parse");

        assert!(config.mise.include_global);
    }

    #[test]
    fn test_parse_host_config() {
        let config = parse_app_config_from_str(
            r#"
[hosts.devbox-1]
address = "user@devbox.example.com"
max_sessions = 12
warn_cpu_percent = 70
warn_memory_percent = 75
idle_detach_minutes = 60
tmux_backed = true

[hosts.local]
max_sessions = 4
"#,
        )
        .expect("config should parse");

        assert_eq!(config.hosts.len(), 2);

        let devbox = config
            .hosts
            .iter()
            .find(|h| h.name == "devbox-1")
            .expect("devbox-1 should exist");
        assert_eq!(
            devbox.ssh_target.as_deref(),
            Some("user@devbox.example.com")
        );
        assert_eq!(devbox.max_sessions, 12);
        assert_eq!(devbox.warn_cpu_percent, 70);
        assert_eq!(devbox.warn_memory_percent, 75);
        assert_eq!(devbox.idle_detach_minutes, 60);
        assert!(devbox.tmux_backed);

        let local = config
            .hosts
            .iter()
            .find(|h| h.name == "local")
            .expect("local should exist");
        assert!(local.ssh_target.is_none());
        assert_eq!(local.max_sessions, 4);
        // defaults
        assert_eq!(local.warn_cpu_percent, 80);
        assert_eq!(local.warn_memory_percent, 80);
        assert_eq!(local.idle_detach_minutes, 120);
        assert!(local.tmux_backed);
    }

    #[test]
    fn parses_http_config_from_toml() {
        let config = parse_app_config_from_str(
            r#"
[http]
enabled = true
port = 9090
bind_address = "0.0.0.0"
unsafe_allow_non_loopback = true
"#,
        )
        .expect("config should parse");
        assert!(config.http.enabled);
        assert_eq!(config.http.port, 9090);
        assert_eq!(config.http.bind_address, "0.0.0.0");
        assert!(config.http.unsafe_allow_non_loopback);
    }

    #[test]
    fn parses_http_control_config_from_toml() {
        let config = parse_app_config_from_str(
            r#"
[http_control]
enabled = true
"#,
        )
        .expect("config should parse");

        assert!(config.http_control.enabled);
    }

    #[test]
    fn http_config_defaults_when_absent() {
        let config = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!config.http.enabled);
        assert_eq!(config.http.port, 7800);
        assert_eq!(config.http.bind_address, "127.0.0.1");
        assert!(!config.http.unsafe_allow_non_loopback);
        assert!(!config.http_control.enabled);
    }

    #[test]
    fn http_non_loopback_opt_in_defaults_to_false() {
        let config = parse_app_config_from_str(
            r#"
[http]
enabled = true
bind_address = "0.0.0.0"
"#,
        )
        .expect("config should parse");
        assert!(!config.http.unsafe_allow_non_loopback);
    }

    #[test]
    fn test_parse_no_hosts_returns_empty() {
        let config = parse_app_config_from_str(
            r#"
[sidebar]
position = "left"
"#,
        )
        .expect("config should parse");
        assert!(config.hosts.is_empty());
    }

    #[test]
    fn workspace_icons_prefer_exact_overrides_then_derived_defaults() {
        let config = parse_app_config_from_str(
            r#"
[sidebar.workspace-icons]
worktree = "⑂"

[sidebar.workspace-icons.by-name]
infra = "⚙"

[sidebar.workspace-icons.by-repo]
taarof = "⌘"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .resolve("infra", Some("/repos/whatever"), false),
            "⚙"
        );
        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .resolve("anything", Some("/repos/taarof"), false),
            "⌘"
        );
        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .resolve("feature", Some("/repos/branchy"), true),
            "⑂"
        );
        assert_eq!(
            config
                .sidebar
                .workspace_icons
                .resolve("notes", Some("/repos/scratch"), false),
            "S"
        );
    }

    #[test]
    fn live_config_reload_reloads_app_and_ghostty_snapshots_together() {
        let dir = new_test_dir();
        let app_path = dir.join("taarof.toml");
        let ghostty_path = dir.join("ghostty-config");

        fs::write(
            &app_path,
            r#"
[terminal]
copy_recent_lines = 320

[tmux]
enabled = true
session_prefix = "alpha"

[hosts.devbox]
address = "dev@box"
"#,
        )
        .expect("app config should be written");
        fs::write(
            &ghostty_path,
            r#"
font-family = "Fira Code"
font-size = 13
copy-on-select = true
command = "/usr/bin/fish"
"#,
        )
        .expect("ghostty config should be written");

        let first = LiveConfigSnapshot::load_from_paths(&app_path, &ghostty_path);
        assert_eq!(first.app.terminal.copy_recent_lines, 320);
        assert!(first.app.tmux.enabled);
        assert_eq!(first.app.tmux.session_prefix, "alpha");
        assert_eq!(first.app.hosts.len(), 1);
        assert_eq!(first.ghostty.font_family, "Fira Code");
        assert_eq!(first.ghostty.font_size, 13.0);
        assert!(first.ghostty.copy_on_select);
        assert_eq!(
            first.ghostty.shell_command.as_deref(),
            Some("/usr/bin/fish")
        );

        fs::write(
            &app_path,
            r#"
[terminal]
copy_recent_lines = 48

[tmux]
enabled = false
session_prefix = "beta"

[http]
enabled = true
port = 8123
"#,
        )
        .expect("updated app config should be written");
        fs::write(
            &ghostty_path,
            r#"
font-family = "Iosevka"
font-size = 15
copy-on-select = false
window-padding-x = 6
"#,
        )
        .expect("updated ghostty config should be written");

        let second = LiveConfigSnapshot::load_from_paths(&app_path, &ghostty_path);
        assert_eq!(second.app.terminal.copy_recent_lines, 48);
        assert!(!second.app.tmux.enabled);
        assert_eq!(second.app.tmux.session_prefix, "beta");
        assert!(second.app.hosts.is_empty());
        assert!(second.app.http.enabled);
        assert_eq!(second.app.http.port, 8123);
        assert_eq!(second.ghostty.font_family, "Iosevka");
        assert_eq!(second.ghostty.font_size, 15.0);
        assert!(!second.ghostty.copy_on_select);
        assert_eq!(second.ghostty.padding_x, 6);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_config_reload_falls_back_to_defaults_for_missing_or_invalid_files() {
        let dir = new_test_dir();
        let app_path = dir.join("taarof.toml");
        let ghostty_path = dir.join("ghostty-config");

        fs::write(&app_path, "[tmux\nenabled = true\n").expect("broken config should be written");

        let snapshot = LiveConfigSnapshot::load_from_paths(&app_path, &ghostty_path);
        assert_eq!(snapshot.app.tmux.enabled, AppConfig::default().tmux.enabled);
        assert_eq!(
            snapshot.app.tmux.session_prefix,
            AppConfig::default().tmux.session_prefix
        );
        assert!(snapshot.app.hosts.is_empty());
        assert!(snapshot.app.history.config_error.is_some());
        assert_eq!(
            snapshot.ghostty.font_family,
            GhosttyConfig::default().font_family
        );
        assert_eq!(
            snapshot.ghostty.shell_command,
            GhosttyConfig::default().shell_command
        );

        fs::write(
            &ghostty_path,
            r#"
font-family = "MonoLisa"
command = "/bin/zsh"
"#,
        )
        .expect("ghostty config should be written");

        let mixed = LiveConfigSnapshot::load_from_paths(&app_path, &ghostty_path);
        assert_eq!(mixed.app.tmux.enabled, AppConfig::default().tmux.enabled);
        assert!(mixed.app.history.config_error.is_some());
        assert_eq!(
            mixed.app.tmux.session_prefix,
            AppConfig::default().tmux.session_prefix
        );
        assert_eq!(mixed.ghostty.font_family, "MonoLisa");
        assert_eq!(mixed.ghostty.shell_command.as_deref(), Some("/bin/zsh"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validated_live_reload_installs_the_captured_app_buffer() {
        let dir = new_test_dir();
        let app_path = dir.join("config.toml");
        let ghostty_path = dir.join("ghostty-config");
        fs::write(
            &app_path,
            "[terminal]\ncopy_recent_lines = 111\n[tmux]\nsession_prefix = \"captured\"\n",
        )
        .expect("initial app config should be written");
        fs::write(&ghostty_path, "font-family = Captured Mono\n")
            .expect("ghostty config should be written");

        let captured = capture_app_config(&app_path).expect("app config should be captured");
        fs::write(
            &app_path,
            "[terminal]\ncopy_recent_lines = 222\n[tmux]\nsession_prefix = \"later\"\n",
        )
        .expect("later app config should be written");

        let installed = std::cell::RefCell::new(None);
        let snapshot = install_live_config_from_capture(captured, &ghostty_path, |snapshot| {
            *installed.borrow_mut() = Some(snapshot.clone());
        })
        .expect("captured config should validate, load, and install");
        let installed = installed
            .into_inner()
            .expect("snapshot should be installed");
        assert_eq!(snapshot.app.terminal.copy_recent_lines, 111);
        assert_eq!(installed.app.terminal.copy_recent_lines, 111);
        assert_eq!(installed.app.tmux.session_prefix, "captured");
        assert_eq!(installed.ghostty.font_family, "Captured Mono");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn captured_invalid_config_returns_findings_without_a_snapshot() {
        let installed = std::cell::Cell::new(false);
        let result = install_live_config_from_capture(
            CapturedAppConfig::Contents("[tmux\nenabled = true\n".to_string()),
            Path::new("/definitely/missing/ghostty-config"),
            |_| installed.set(true),
        );
        let findings = result.expect_err("invalid capture must not produce a snapshot");
        assert!(!installed.get(), "invalid captures must not be installed");
        assert!(findings.iter().any(|finding| {
            finding.severity == Severity::Error && finding.message.contains("parse error")
        }));
    }

    #[test]
    fn app_config_editor_open_command_parses_and_defaults() {
        let parsed =
            parse_app_config_from_str("[editor]\nopen_command = \"code -g {path}:{line}\"\n")
                .expect("parse");
        assert_eq!(parsed.editor.open_command, "code -g {path}:{line}");

        let defaulted = parse_app_config_from_str("").expect("parse empty");
        assert_eq!(defaulted.editor.open_command, "zed {path}:{line}:{col}");
    }

    #[test]
    fn app_config_editor_click_action_parses_and_defaults() {
        let editor = parse_app_config_from_str("[editor]\nclick_action = \"editor\"\n")
            .expect("parse editor click_action")
            .editor
            .click_action;
        assert_eq!(editor, ClickAction::Editor);

        let peek = parse_app_config_from_str("[editor]\nclick_action = \"peek\"\n")
            .expect("parse peek click_action")
            .editor
            .click_action;
        assert_eq!(peek, ClickAction::Peek);

        // Missing, blank, and unknown values all default to peek.
        assert_eq!(
            parse_app_config_from_str("")
                .expect("parse empty")
                .editor
                .click_action,
            ClickAction::Peek
        );
        assert_eq!(
            parse_app_config_from_str("[editor]\nclick_action = \"\"\n")
                .expect("parse blank")
                .editor
                .click_action,
            ClickAction::Peek
        );
        assert_eq!(
            parse_app_config_from_str("[editor]\nclick_action = \"bogus\"\n")
                .expect("parse bogus")
                .editor
                .click_action,
            ClickAction::Peek
        );
    }

    #[test]
    fn app_config_browser_open_command_parses_and_defaults() {
        let configured =
            parse_app_config_from_str("[browser]\nopen_command = \"helium-browser {url}\"\n")
                .expect("parse browser open_command");
        assert_eq!(
            configured.browser.open_command.as_deref(),
            Some("helium-browser {url}")
        );

        // Missing section and blank values both mean "system default browser".
        assert_eq!(
            parse_app_config_from_str("")
                .expect("parse empty")
                .browser
                .open_command,
            None
        );
        assert_eq!(
            parse_app_config_from_str("[browser]\nopen_command = \"   \"\n")
                .expect("parse blank")
                .browser
                .open_command,
            None
        );
    }

    #[test]
    fn app_config_editor_open_command_blank_values_fall_back_to_default() {
        let empty = parse_app_config_from_str("[editor]\nopen_command = \"\"\n")
            .expect("parse empty open_command");
        assert_eq!(empty.editor.open_command, "zed {path}:{line}:{col}");

        let whitespace = parse_app_config_from_str("[editor]\nopen_command = \"   \"\n")
            .expect("parse whitespace open_command");
        assert_eq!(whitespace.editor.open_command, "zed {path}:{line}:{col}");
    }

    #[test]
    fn app_config_terminal_copy_recent_lines_uses_default_for_invalid_values() {
        let parsed = parse_app_config_from_str("[terminal]\ncopy_recent_lines = 512\n")
            .expect("valid terminal config should parse");
        assert_eq!(parsed.terminal.copy_recent_lines, 512);

        let zero = parse_app_config_from_str("[terminal]\ncopy_recent_lines = 0\n")
            .expect("zero-valued terminal config should still parse");
        assert_eq!(
            zero.terminal.copy_recent_lines,
            TerminalConfig::default().copy_recent_lines
        );

        let oversized = parse_app_config_from_str(&format!(
            "[terminal]\ncopy_recent_lines = {}\n",
            crate::terminal::MAX_CAPTURE_SCROLLBACK_LINES + 1
        ))
        .expect("oversized terminal config should still parse");
        assert_eq!(
            oversized.terminal.copy_recent_lines,
            TerminalConfig::default().copy_recent_lines
        );
    }

    #[test]
    fn app_config_terminal_clipboard_history_size_uses_default_for_invalid_values() {
        let parsed = parse_app_config_from_str("[terminal]\nclipboard_history_size = 12\n")
            .expect("valid clipboard_history_size should parse");
        assert_eq!(parsed.terminal.clipboard_history_size, 12);

        let zero = parse_app_config_from_str("[terminal]\nclipboard_history_size = 0\n")
            .expect("zero clipboard_history_size should still parse");
        assert_eq!(
            zero.terminal.clipboard_history_size,
            TerminalConfig::default().clipboard_history_size
        );

        let oversized = parse_app_config_from_str(&format!(
            "[terminal]\nclipboard_history_size = {}\n",
            MAX_CLIPBOARD_HISTORY_SIZE + 1
        ))
        .expect("oversized clipboard_history_size should still parse");
        assert_eq!(
            oversized.terminal.clipboard_history_size,
            TerminalConfig::default().clipboard_history_size
        );

        let defaulted = parse_app_config_from_str("[terminal]\ncopy_recent_lines = 200\n")
            .expect("terminal config without clipboard_history_size should parse");
        assert_eq!(
            defaulted.terminal.clipboard_history_size,
            TerminalConfig::default().clipboard_history_size
        );
    }

    #[test]
    fn history_config_defaults_off_and_parses_all_limits() {
        let defaults = parse_app_config_from_str("").expect("empty config should parse");
        assert!(!defaults.history.enabled);
        assert_eq!(defaults.history.max_age_days, 30);
        assert_eq!(defaults.history.max_records, 200_000);
        assert_eq!(defaults.history.max_bytes, 256 * 1024 * 1024);
        assert_eq!(defaults.history.maintenance_interval_minutes, 15);
        assert_eq!(defaults.history.queue_capacity, 4_096);
        assert!(defaults.history.validate().is_ok());

        let parsed = parse_app_config_from_str(
            "[history]\nenabled = true\nmax_age_days = 7\nmax_records = 1000\nmax_bytes = 16777216\nmaintenance_interval_minutes = 7\nqueue_capacity = 128\nrecord_events = false\nrecord_diagnostics = false\nrecord_work = true\n",
        )
        .expect("history config should parse");
        assert!(parsed.history.enabled);
        assert_eq!(parsed.history.max_age_days, 7);
        assert_eq!(parsed.history.max_records, 1_000);
        assert_eq!(parsed.history.max_bytes, 16_777_216);
        assert_eq!(parsed.history.maintenance_interval_minutes, 7);
        assert_eq!(parsed.history.queue_capacity, 128);
        assert!(!parsed.history.record_events);
        assert!(!parsed.history.record_diagnostics);
        assert!(parsed.history.record_work);
    }

    #[test]
    fn invalid_history_config_fails_with_actionable_messages() {
        let cases = [
            ("max_age_days = 3651", "history.max_age_days is 3651"),
            ("max_records = 999", "history.max_records is 999"),
            ("max_bytes = 1024", "history.max_bytes is 1024"),
            (
                "maintenance_interval_minutes = 0",
                "history.maintenance_interval_minutes is 0",
            ),
            ("queue_capacity = 63", "history.queue_capacity is 63"),
        ];
        for (setting, expected) in cases {
            let error = parse_app_config_from_str(&format!("[history]\n{setting}\n"))
                .expect_err("invalid history config must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }

        let no_classes = parse_app_config_from_str(
            "[history]\nenabled = true\nrecord_events = false\nrecord_diagnostics = false\nrecord_work = false\n",
        )
        .expect_err("enabled history needs a record class");
        assert!(no_classes
            .to_string()
            .contains("enable at least one record class"));

        let unbounded = parse_app_config_from_str(
            "[history]\nenabled = true\nmax_age_days = 0\nmax_records = 0\nmax_bytes = 0\n",
        )
        .expect_err("enabled history needs a retention bound");
        assert!(unbounded
            .to_string()
            .contains("configure at least one retention bound"));
    }

    #[test]
    fn config_validator_reports_invalid_history_even_when_disabled() {
        let findings = validate_app_config_str("[history]\nmax_records = 42\n");
        assert!(findings.iter().any(|finding| {
            finding.severity == Severity::Error
                && finding.message.contains("history.max_records is 42")
        }));
    }

    #[test]
    fn invalid_loaded_history_config_surfaces_as_misconfigured() {
        let path = std::env::temp_dir().join(format!(
            "taarof-invalid-history-config-{}-{}.toml",
            std::process::id(),
            crate::history::now_unix_ms()
        ));
        std::fs::write(
            &path,
            "[terminal]\ncopy_recent_lines = 321\n\n[hosts.devbox]\naddress = \"dev@box\"\n\n[history]\nenabled = true\nmax_records = 42\n",
        )
        .unwrap();
        let config = AppConfig::load_from_path(&path);
        assert!(config.history.config_error.is_some());
        assert_eq!(config.terminal.copy_recent_lines, 321);
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].name, "devbox");
        assert_eq!(config.hosts[0].ssh_target.as_deref(), Some("dev@box"));
        let (history, _) = crate::history::HistoryHandle::open(config.history);
        assert_eq!(history.status().state, "misconfigured");
        assert!(history
            .status()
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("history.max_records is 42")));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_installed_binary_path_requires_an_absolute_non_empty_path() {
        let parsed = parse_app_config_from_str(
            "[update]\ninstalled_binary_path = \"/opt/taarof/taarof-app\"\n",
        )
        .expect("absolute update path should parse");
        assert_eq!(
            parsed.update.installed_binary_path,
            Some(PathBuf::from("/opt/taarof/taarof-app"))
        );

        for value in ["", "relative/taarof-app", "   "] {
            let parsed = parse_app_config_from_str(&format!(
                "[update]\ninstalled_binary_path = {value:?}\n"
            ))
            .expect("invalid update path should fall through to defaults");
            assert!(parsed.update.installed_binary_path.is_none());
        }
    }
}
