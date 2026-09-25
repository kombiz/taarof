/// Discover mise tasks by shelling out to `mise tasks ls --json`.
/// Returns an empty vec if mise is not installed or no tasks are found.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

mod cache;
mod chip;
mod local;
mod remote;

pub use self::remote::shell_quote;

pub(crate) use self::cache::{cached_task_discovery, spawn_task_discovery};
pub(crate) use self::chip::tool_version_chip_text_for_target;
#[cfg(test)]
pub(crate) use self::local::local_discovery_target;
pub(crate) use self::local::update_discovery_binary_cache;

use self::local::{
    best_discovery_dir, discover_tasks_with_binary, filter_tasks_for_target, has_mise_config,
    nearest_mise_config_dir, require_local_mise_binary,
};
use self::remote::{
    remote_mise_shell_command, remote_shell_respawn_command, ssh_command_with_remote_exec,
    ssh_destination_index, ssh_supports_remote_exec,
};

#[cfg(test)]
use self::remote::remote_mise_run_command;

#[cfg(test)]
use self::local::{classify_task_scope, find_mise, normalize_mise_binary_path};

#[cfg(test)]
use self::cache::spawn_task_discovery_for_test;
#[cfg(test)]
pub(crate) use self::cache::{
    clear_task_discovery_cache_for_test, set_pending_task_discovery_for_test,
    set_ready_task_discovery_for_test, task_discovery_test_guard,
};
#[cfg(test)]
pub(crate) use self::chip::{
    clear_tool_version_cache_for_test, clear_tool_version_test_probe,
    install_tool_version_test_probe, tool_version_test_entry, tool_version_test_probe_call_count,
    wait_for_tool_version_chip,
};
#[cfg(test)]
pub(crate) use self::local::{
    clear_find_mise_test_probe, find_mise_test_probe_call_count, install_find_mise_test_probe,
};

const MISSING_MISE_BINARY_MESSAGE: &str =
    "mise binary not found in MISE_BIN, PATH, or common locations";
const TASK_DISCOVERY_TTL: Duration = Duration::from_secs(45);
const TOOL_VERSION_TTL: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct MiseTask {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hide: bool,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolVersionEntry {
    tool: String,
    version: String,
    source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MiseTaskScope {
    ProjectRoot,
    ParentDir,
    Global,
}

#[cfg(test)]
mod async_discovery {
    use super::{
        cached_task_discovery, clear_task_discovery_cache_for_test,
        set_ready_task_discovery_for_test, spawn_task_discovery_for_test, CachedTaskDiscovery,
        DiscoveryTarget, MiseTask, TaskDiscoveryRequest, TASK_DISCOVERY_TTL,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    fn wait_for_ready_tasks(target: &DiscoveryTarget) -> Vec<MiseTask> {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match cached_task_discovery(target) {
                CachedTaskDiscovery::Ready(tasks) => return tasks,
                CachedTaskDiscovery::Pending if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                state => panic!("timed out waiting for cached async discovery result: {state:?}"),
            }
        }
    }

    #[test]
    fn test_discovery_runs_on_background_thread() {
        let _guard = super::task_discovery_test_guard();
        clear_task_discovery_cache_for_test();

        let target = DiscoveryTarget::Remote {
            host: "example.com".into(),
            cwd: "/srv/app".into(),
            ssh_argv: vec!["ssh".into(), "example.com".into()],
        };
        let caller_thread = std::thread::current().id();
        let (tx, rx) = mpsc::channel();

        let request = spawn_task_discovery_for_test(target.clone(), move |_| {
            tx.send(std::thread::current().id())
                .expect("worker thread id should be sent");
            vec![MiseTask {
                name: "app:test".into(),
                description: "test task".into(),
                hide: false,
                source: None,
                global: false,
            }]
        });

        assert!(matches!(request, TaskDiscoveryRequest::Start));

        let worker_thread = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("background discovery should execute");
        assert_ne!(worker_thread, caller_thread);

        let tasks = wait_for_ready_tasks(&target);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "app:test");
    }

    #[test]
    fn test_discovery_cache_is_scoped_per_target_and_expires() {
        let _guard = super::task_discovery_test_guard();
        clear_task_discovery_cache_for_test();

        let first_target = DiscoveryTarget::Remote {
            host: "example.com".into(),
            cwd: "/srv/app".into(),
            ssh_argv: vec!["ssh".into(), "example.com".into()],
        };
        let second_target = DiscoveryTarget::Remote {
            host: "example.com".into(),
            cwd: "/srv/other".into(),
            ssh_argv: vec!["ssh".into(), "example.com".into()],
        };
        let tasks = vec![MiseTask {
            name: "app:test".into(),
            description: "test task".into(),
            hide: false,
            source: None,
            global: false,
        }];

        set_ready_task_discovery_for_test(
            &first_target,
            tasks.clone(),
            TASK_DISCOVERY_TTL - Duration::from_secs(1),
        );

        assert_eq!(
            cached_task_discovery(&first_target),
            CachedTaskDiscovery::Ready(tasks)
        );
        assert_eq!(
            cached_task_discovery(&second_target),
            CachedTaskDiscovery::Missing
        );

        set_ready_task_discovery_for_test(
            &first_target,
            Vec::new(),
            TASK_DISCOVERY_TTL + Duration::from_secs(1),
        );
        assert_eq!(
            cached_task_discovery(&first_target),
            CachedTaskDiscovery::Missing
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryTarget {
    Local {
        cwd: String,
        binary_path: Option<PathBuf>,
    },
    Remote {
        host: String,
        cwd: String,
        ssh_argv: Vec<String>,
    },
}

/// Whether two targets identify the same task source. The local mise binary is
/// execution plumbing, not project identity; CWD is what binds a local task
/// discovery result to a pane. Remote identities include the bounded SSH argv
/// so aliases, ports, and jump-host routes cannot bleed into one another.
pub(crate) fn same_task_target(left: &DiscoveryTarget, right: &DiscoveryTarget) -> bool {
    match (left, right) {
        (DiscoveryTarget::Local { cwd: left, .. }, DiscoveryTarget::Local { cwd: right, .. }) => {
            left == right
        }
        (
            DiscoveryTarget::Remote {
                host: left_host,
                cwd: left_cwd,
                ssh_argv: left_ssh,
            },
            DiscoveryTarget::Remote {
                host: right_host,
                cwd: right_cwd,
                ssh_argv: right_ssh,
            },
        ) => left_host == right_host && left_cwd == right_cwd && left_ssh == right_ssh,
        _ => false,
    }
}

/// A safe, actionable label for a discovery target. SSH argv is intentionally
/// omitted because it can include identity paths and proxy routing details.
pub(crate) fn task_target_label(target: &DiscoveryTarget) -> String {
    match target {
        DiscoveryTarget::Local { cwd, .. } => format!("local project {cwd}"),
        DiscoveryTarget::Remote { host, cwd, .. } => format!("remote project {host}:{cwd}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiscoveryCacheKey {
    location: DiscoveryCacheLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DiscoveryCacheLocation {
    Local {
        cwd: String,
    },
    Remote {
        host: String,
        cwd: String,
        ssh_argv: Vec<String>,
    },
}

#[derive(Debug, Clone)]
enum TaskDiscoveryCacheEntry {
    Pending,
    Ready {
        tasks: Vec<MiseTask>,
        completed_at: Instant,
    },
}

#[derive(Debug, Clone)]
enum ToolVersionCacheEntry {
    Pending,
    Ready {
        chip: Option<String>,
        completed_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolVersionDiscoveryRequest {
    UseCached,
    Pending,
    Start,
}

#[derive(Debug, Default)]
struct TaskDiscoveryCache {
    entries: std::collections::HashMap<DiscoveryCacheKey, TaskDiscoveryCacheEntry>,
}

#[derive(Debug, Default)]
struct ToolVersionCache {
    entries: std::collections::HashMap<DiscoveryCacheKey, ToolVersionCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachedTaskDiscovery {
    Missing,
    Pending,
    Ready(Vec<MiseTask>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskDiscoveryRequest {
    UseCached,
    Pending,
    Start,
}

impl From<&DiscoveryTarget> for DiscoveryCacheKey {
    fn from(target: &DiscoveryTarget) -> Self {
        let location = match target {
            DiscoveryTarget::Local { cwd, .. } => {
                DiscoveryCacheLocation::Local { cwd: cwd.clone() }
            }
            DiscoveryTarget::Remote {
                host,
                cwd,
                ssh_argv,
            } => DiscoveryCacheLocation::Remote {
                host: host.clone(),
                cwd: cwd.clone(),
                ssh_argv: ssh_argv.clone(),
            },
        };
        Self { location }
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct DiscoverTasksTestProbe {
    target: DiscoveryTarget,
    delay: Duration,
    result: Vec<MiseTask>,
    calls: usize,
}

#[cfg(test)]
fn discover_tasks_test_probe() -> &'static Mutex<Option<DiscoverTasksTestProbe>> {
    static PROBE: OnceLock<Mutex<Option<DiscoverTasksTestProbe>>> = OnceLock::new();
    PROBE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_discover_tasks_test_probe(target: &DiscoveryTarget) -> Option<Vec<MiseTask>> {
    let (delay, result) = {
        let mut probe = discover_tasks_test_probe()
            .lock()
            .expect("discover tasks test probe lock should not be poisoned");
        let probe = probe.as_mut()?;
        if probe.target != *target {
            return None;
        }
        probe.calls += 1;
        (probe.delay, probe.result.clone())
    };
    std::thread::sleep(delay);
    Some(filter_tasks_for_target(result, target))
}

#[cfg(test)]
pub(crate) fn install_discover_tasks_test_probe(
    target: DiscoveryTarget,
    result: Vec<MiseTask>,
    delay: Duration,
) {
    discover_tasks_test_probe()
        .lock()
        .expect("discover tasks test probe lock should not be poisoned")
        .replace(DiscoverTasksTestProbe {
            target,
            delay,
            result,
            calls: 0,
        });
}

#[cfg(test)]
pub(crate) fn clear_discover_tasks_test_probe() {
    discover_tasks_test_probe()
        .lock()
        .expect("discover tasks test probe lock should not be poisoned")
        .take();
}

#[cfg(test)]
pub(crate) fn discover_tasks_test_probe_call_count() -> usize {
    discover_tasks_test_probe()
        .lock()
        .expect("discover tasks test probe lock should not be poisoned")
        .as_ref()
        .map(|probe| probe.calls)
        .unwrap_or(0)
}

fn trimmed_non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn tmux_backing_cwd(backing: &crate::pane::TmuxBacking) -> Option<String> {
    backing
        .pane_info
        .value()
        .and_then(|info| trimmed_non_empty(&info.cwd))
}

fn pane_leaf_discovery_cwd(leaf: &crate::pane::PaneLeaf) -> Option<String> {
    leaf.location_state
        .cwd
        .as_deref()
        .and_then(trimmed_non_empty)
        .or_else(|| leaf.tmux_backing.as_ref().and_then(tmux_backing_cwd))
}

fn pane_leaf_discovery_host(leaf: &crate::pane::PaneLeaf) -> Option<String> {
    leaf.location_state
        .cwd_host
        .as_deref()
        .and_then(trimmed_non_empty)
        .or_else(|| {
            leaf.tmux_backing
                .as_ref()
                .and_then(|backing| backing.target.ssh_target_string())
        })
}

/// The default working directory to probe on a remote host when taarof has an
/// SSH destination but no reliable remote cwd (e.g. the remote never emitted
/// OSC 7). `.` resolves to the login shell's home on the far side; discovery may
/// then legitimately find no `.plan/tasks.json`, which surfaces an empty state
/// rather than silently doing nothing.
const REMOTE_DEFAULT_CWD: &str = ".";

/// True when `host` names the local machine (self-host aliases, loopback, or the
/// system hostname). Manual-SSH detection must reject these: a local shell can
/// emit its own hostname over OSC 7 or via its window title (e.g. a
/// `user@build-host:~/repo` PS1), and treating that as remote would trigger a
/// doomed SSH-to-self instead of a normal local discovery.
fn host_is_local(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
    {
        return true;
    }
    let local: String = glib::host_name().into();
    let local_short = local.split('.').next().unwrap_or(&local);
    let host_short = host.split('.').next().unwrap_or(host);
    local_short.eq_ignore_ascii_case(host_short)
}

/// Extract the hostname from an ssh argv's destination positional, stripping any
/// `user@` prefix. Reuses [`ssh_destination_index`] so port/identity/option flags
/// are skipped the same way the remote-exec builder skips them. Returns `None`
/// when the argv has no destination or the destination is empty after stripping.
fn ssh_destination_host(argv: &[String]) -> Option<String> {
    let idx = ssh_destination_index(argv)?;
    let destination = argv.get(idx)?.trim();
    let host = destination
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(destination)
        .trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// The discovery-relevant signals taarof has recorded for a single pane, decoupled
/// from the GTK `PaneLeaf` so [`resolve_discovery_target_from_signals`] can be unit
/// tested without constructing a terminal widget.
#[derive(Debug, Default, Clone)]
struct PaneDiscoverySignals {
    /// Recorded cwd (OSC 7 path, tmux pane cwd, or title-parsed path).
    cwd: Option<String>,
    /// Recorded remote host (OSC 7 host, tmux ssh target, or title-parsed host).
    cwd_host: Option<String>,
    /// Full ssh argv captured from the pane's process tree, when the process
    /// probe found an ssh child (covers manually-run `ssh host`).
    ssh_command: Option<Vec<String>>,
    /// The process probe classified this pane's foreground process as a remote
    /// shell (e.g. an ssh session), even without a parsed argv.
    remote_shell: bool,
}

impl PaneDiscoverySignals {
    fn from_leaf(leaf: &crate::pane::PaneLeaf) -> Self {
        Self {
            cwd: pane_leaf_discovery_cwd(leaf),
            cwd_host: pane_leaf_discovery_host(leaf),
            ssh_command: leaf.ssh_command(),
            remote_shell: leaf.process_state.remote_shell,
        }
    }
}

/// Resolve a pane's recorded signals to a discovery target, honoring this
/// precedence (EXAMPLE-33):
///
/// a. A non-local `cwd_host` (OSC 7 / title / tmux target) plus a recorded cwd →
///    remote target at that host and cwd. This is the reliable path when the
///    remote emits OSC 7 (the taarof-remote-info-setup flow). Local/self hostnames
///    are rejected here so a local shell that advertises its own hostname stays
///    local.
/// b. Otherwise, a manually-run ssh detected by the process probe
///    (`ssh_command` / `remote_shell`) → remote target at the ssh destination
///    host, using the remote default cwd (the recorded cwd is the *local*
///    pre-ssh directory and must not be replayed as a remote path).
/// c. Otherwise → local target at the recorded cwd.
///
/// Returns `None` only when no cwd is available for a local target and no remote
/// destination could be resolved — the caller surfaces that as a user-visible
/// "no target" message rather than a silent no-op.
fn resolve_discovery_target_from_signals(
    signals: &PaneDiscoverySignals,
    fallback_cwd: Option<String>,
    binary_path: Option<PathBuf>,
) -> Option<DiscoveryTarget> {
    let cwd = signals.cwd.clone().or(fallback_cwd);

    // (a) Reliable remote metadata: a non-local host plus a known remote cwd.
    if let Some(host) = signals
        .cwd_host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty() && !host_is_local(host))
    {
        if let Some(cwd) = cwd.clone() {
            let ssh_argv = signals
                .ssh_command
                .clone()
                .filter(|argv| ssh_supports_remote_exec(argv))
                .unwrap_or_else(|| vec!["ssh".into(), host.to_string()]);
            return Some(DiscoveryTarget::Remote {
                host: host.to_string(),
                cwd,
                ssh_argv,
            });
        }
    }

    // (b) Manual ssh detected by the process probe but no reliable remote cwd:
    // reach the far side via the parsed ssh destination and probe its default dir.
    if signals.remote_shell {
        if let Some(argv) = signals
            .ssh_command
            .as_ref()
            .filter(|argv| ssh_supports_remote_exec(argv))
        {
            if let Some(host) = ssh_destination_host(argv).filter(|host| !host_is_local(host)) {
                return Some(DiscoveryTarget::Remote {
                    host,
                    cwd: REMOTE_DEFAULT_CWD.to_string(),
                    ssh_argv: argv.clone(),
                });
            }
        }
    }

    // (c) Plain local pane.
    Some(DiscoveryTarget::Local {
        cwd: cwd?,
        binary_path,
    })
}

fn target_for_tab(
    tab: &crate::workspace::Tab,
    binary_path: Option<PathBuf>,
) -> Option<DiscoveryTarget> {
    let leaves = tab.panes.leaves();
    let leaf = leaves
        .iter()
        .copied()
        .find(|leaf| leaf.pane_id == tab.focused_pane_id)
        .or_else(|| leaves.first().copied());
    let signals = leaf
        .map(PaneDiscoverySignals::from_leaf)
        .unwrap_or_default();
    resolve_discovery_target_from_signals(&signals, tab.discovery_cwd.clone(), binary_path)
}

/// Why [`target_for_tab`] could not build a [`DiscoveryTarget`]. Callers turn this
/// into user-visible feedback so an explicit "Discover Tasks" action is never a
/// silent no-op (EXAMPLE-33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryTargetFailure {
    /// A local pane whose shell has not reported a working directory yet.
    NoWorkingDirectory,
    /// A remote session was detected (OSC 7 host, or an ssh process in the pane)
    /// but taarof could not resolve a reachable host and directory to probe.
    UnresolvedRemoteContext,
}

/// Classify why a pane's signals did not yield a target. Mirrors the precedence
/// in [`resolve_discovery_target_from_signals`]: any remote indicator that failed
/// to resolve reads as unresolved remote context; otherwise it is a plain local
/// pane that has not reported a directory.
fn classify_unresolved_signals(signals: &PaneDiscoverySignals) -> DiscoveryTargetFailure {
    let has_remote_signal = signals.remote_shell
        || signals
            .ssh_command
            .as_deref()
            .is_some_and(|argv| !argv.is_empty())
        || signals
            .cwd_host
            .as_deref()
            .map(str::trim)
            .is_some_and(|host| !host.is_empty() && !host_is_local(host));

    if has_remote_signal {
        DiscoveryTargetFailure::UnresolvedRemoteContext
    } else {
        DiscoveryTargetFailure::NoWorkingDirectory
    }
}

/// Classify why [`target_for_tab`] returned `None` for `tab`, using the same
/// focused-leaf selection as target resolution.
pub(crate) fn classify_discovery_target_failure(
    tab: &crate::workspace::Tab,
) -> DiscoveryTargetFailure {
    let leaves = tab.panes.leaves();
    let leaf = leaves
        .iter()
        .copied()
        .find(|leaf| leaf.pane_id == tab.focused_pane_id)
        .or_else(|| leaves.first().copied());
    let signals = leaf
        .map(PaneDiscoverySignals::from_leaf)
        .unwrap_or_default();
    classify_unresolved_signals(&signals)
}

pub(crate) fn discovery_target_for_workspace(
    state: &crate::AppState,
    ws_id: u32,
) -> Option<DiscoveryTarget> {
    let workspace = state
        .workspaces
        .iter()
        .find(|workspace| workspace.id == ws_id)?;

    let workspace_root_target = workspace
        .working_tree_path
        .clone()
        .or_else(|| workspace.repo_root.clone())
        .map(|cwd| DiscoveryTarget::Local {
            cwd,
            binary_path: None,
        });

    if workspace.host_config_name.is_none() {
        if let Some(target) = workspace_root_target.clone() {
            return Some(target);
        }
    }

    if let Some(tab) = workspace
        .tabs
        .iter()
        .find(|tab| tab.id == workspace.active_tab && tab.kind == crate::TabKind::Terminal)
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.kind == crate::TabKind::Terminal)
        })
    {
        let binary_path = state.discovery_binary_paths.get(&tab.id).cloned();
        if let Some(target) = target_for_tab(tab, binary_path) {
            return Some(target);
        }
    }

    workspace_root_target
}

pub(crate) fn discovery_target_for_tab(
    state: &crate::AppState,
    tab_id: u32,
) -> Option<DiscoveryTarget> {
    let (ws, tab) = state.find_tab(tab_id)?;
    if ws.id != state.active_workspace {
        return None;
    }
    target_for_tab(tab, state.discovery_binary_paths.get(&tab.id).cloned())
}

/// Resolve the exact normalized target used by task discovery for one tab.
/// Task-launch planning must use this same normalization before it compares a
/// rendered affordance with the current pane; otherwise a nested local CWD
/// could spuriously look stale against the discovered mise project root.
pub(crate) fn task_target_for_tab(state: &crate::AppState, tab_id: u32) -> Option<DiscoveryTarget> {
    match discovery_target_for_tab(state, tab_id)? {
        DiscoveryTarget::Local { cwd, binary_path } => Some(DiscoveryTarget::Local {
            cwd: local_discovery_dir_from(&cwd),
            binary_path,
        }),
        remote => Some(remote),
    }
}

fn focused_target(state: &crate::AppState) -> Option<DiscoveryTarget> {
    let tab_id = state.active_tab()?.id;
    discovery_target_for_tab(state, tab_id)
}

/// Resolve the best local discovery directory from a given CWD.
/// Uses the nearest mise-configured ancestor when one exists, otherwise keeps
/// the pane's own CWD. Never fall back to taarof's process CWD or HOME here:
/// those are ambient app state, not context for the tab the user selected.
pub fn local_discovery_dir_from(cwd: &str) -> String {
    let cwd = PathBuf::from(cwd);
    nearest_mise_config_dir(&cwd)
        .unwrap_or(cwd)
        .to_string_lossy()
        .into_owned()
}

fn local_discovery_dir(state: &crate::AppState, initial: Option<&str>) -> String {
    let mut candidates = Vec::new();

    if let Some(path) = initial {
        candidates.push(PathBuf::from(path));
    }

    if let Some(ws) = state.active_ws() {
        if let Some(ref root) = ws.repo_root {
            candidates.push(PathBuf::from(root));
        }
    }

    if let Ok(dir) = std::env::current_dir() {
        candidates.push(dir);
    }

    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home));
    }

    best_discovery_dir(candidates)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

fn discover_remote_tasks(host: &str, cwd: &str, ssh_argv: &[String]) -> Vec<MiseTask> {
    let remote_command = remote_mise_shell_command(cwd, &["tasks", "ls", "--json"]);
    let Some(argv) =
        ssh_command_with_remote_exec(ssh_argv, remote_command, false, &["-o", "BatchMode=yes"])
    else {
        crate::diagnostics::record_command_failure(
            "mise",
            "remote-discover",
            format!("remote mise unsupported for {host}:{cwd}"),
            Some(serde_json::json!({
                "host": host,
                "cwd": cwd,
            })),
        );
        return Vec::new();
    };

    let output = Command::new(&argv[0]).args(&argv[1..]).output();
    match output {
        Ok(out) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                crate::diagnostics::record_command_failure(
                    "mise",
                    "remote-discover",
                    format!("remote mise failed in {host}:{cwd}"),
                    Some(serde_json::json!({
                        "host": host,
                        "cwd": cwd,
                        "status": out.status.to_string(),
                        "stderr": stderr.trim(),
                        "argv": argv,
                    })),
                );
                return Vec::new();
            }
            let tasks: Vec<MiseTask> = serde_json::from_slice(&out.stdout).unwrap_or_default();
            eprintln!("taarof: mise found {} tasks in {host}:{cwd}", tasks.len());
            tasks.into_iter().filter(|t| !t.hide).collect()
        }
        Err(e) => {
            crate::diagnostics::record_command_failure(
                "mise",
                "remote-discover",
                format!("remote mise failed to run for {host}:{cwd}"),
                Some(serde_json::json!({
                    "host": host,
                    "cwd": cwd,
                    "argv": argv,
                    "error": e.to_string(),
                })),
            );
            Vec::new()
        }
    }
}

/// Discover mise tasks for a given directory.
#[cfg(test)]
pub fn discover_tasks(cwd: &str) -> Vec<MiseTask> {
    // Validate directory exists before spawning
    if !Path::new(cwd).is_dir() {
        crate::diagnostics::record_command_failure(
            "mise",
            "discover",
            format!("mise skipped because the directory does not exist: {cwd}"),
            Some(serde_json::json!({
                "cwd": cwd,
            })),
        );
        return Vec::new();
    }

    discover_tasks_for_target(&local_discovery_target(cwd))
}

pub fn discover_tasks_for_target(target: &DiscoveryTarget) -> Vec<MiseTask> {
    #[cfg(test)]
    if let Some(tasks) = run_discover_tasks_test_probe(target) {
        return tasks;
    }

    let tasks = match target {
        DiscoveryTarget::Local { cwd, binary_path } => {
            let Some(binary_path) = require_local_mise_binary(
                binary_path,
                "discover",
                serde_json::json!({
                    "cwd": cwd,
                }),
            ) else {
                return Vec::new();
            };

            discover_tasks_with_binary(cwd, &binary_path)
        }
        DiscoveryTarget::Remote {
            host,
            cwd,
            ssh_argv,
        } => discover_remote_tasks(host, cwd, ssh_argv),
    };

    filter_tasks_for_target(tasks, target)
}

pub(crate) fn standard_actions_from_tasks(
    tasks: &[MiseTask],
) -> Vec<crate::workspace::WorkspaceAction> {
    let task_names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
    crate::workspace::WorkspaceAction::all()
        .iter()
        .copied()
        .filter(|action| task_names.contains(&action.task_name()))
        .collect()
}

pub(crate) fn task_buttons_from_tasks(
    tasks: &[MiseTask],
    project: Option<&crate::project_config::ProjectConfig>,
) -> Vec<crate::workspace::WorkspaceTaskButton> {
    if let Some(pinned_tasks) = crate::project_config::pinned_task_names(project) {
        return pinned_tasks
            .iter()
            .filter_map(|task_name| {
                tasks
                    .iter()
                    .find(|task| task.name == *task_name)
                    .map(|task| crate::workspace::WorkspaceTaskButton {
                        label: task.name.clone(),
                        task_name: task.name.clone(),
                    })
            })
            .collect();
    }

    standard_actions_from_tasks(tasks)
        .into_iter()
        .map(crate::workspace::WorkspaceTaskButton::from_action)
        .collect()
}

pub fn task_spawn_config(
    target: &DiscoveryTarget,
    task_name: &str,
) -> Option<(Option<String>, Vec<String>)> {
    match target {
        DiscoveryTarget::Local { cwd, binary_path } => {
            let binary_path = require_local_mise_binary(
                binary_path,
                "spawn",
                serde_json::json!({
                    "cwd": cwd,
                    "task_name": task_name,
                }),
            )?;

            Some((
                Some(cwd.clone()),
                vec![
                    binary_path.to_string_lossy().into_owned(),
                    "run".to_string(),
                    "--raw".to_string(),
                    task_name.to_string(),
                ],
            ))
        }
        DiscoveryTarget::Remote { cwd, ssh_argv, .. } => {
            let remote_command = remote_mise_shell_command(cwd, &["run", "--raw", task_name]);
            let argv = ssh_command_with_remote_exec(ssh_argv, remote_command, true, &["-q"])?;
            Some((None, argv))
        }
    }
}

#[cfg(test)]
pub fn task_shell_command(target: &DiscoveryTarget, task_name: &str) -> Option<String> {
    match target {
        DiscoveryTarget::Local { cwd, binary_path } => {
            let binary_path = require_local_mise_binary(
                binary_path,
                "shell-run",
                serde_json::json!({
                    "cwd": cwd,
                    "task_name": task_name,
                }),
            )?;

            Some(format!(
                "{} run --raw {}",
                shell_quote(binary_path.to_string_lossy().as_ref()),
                shell_quote(task_name),
            ))
        }
        DiscoveryTarget::Remote { cwd, .. } => {
            Some(remote_mise_run_command(cwd, &["run", "--raw", task_name]))
        }
    }
}

pub fn task_post_exit_respawn(target: &DiscoveryTarget) -> Option<crate::workspace::ExitRespawn> {
    match target {
        DiscoveryTarget::Remote { cwd, ssh_argv, .. } => {
            let remote_command = remote_shell_respawn_command(cwd);
            let argv = ssh_command_with_remote_exec(ssh_argv, remote_command, true, &["-q"])?;
            Some(crate::workspace::ExitRespawn {
                working_dir: None,
                argv,
            })
        }
        DiscoveryTarget::Local { .. } => None,
    }
}

/// Determine the best target for mise task discovery.
#[allow(deprecated)]
pub fn discovery_target(state: &crate::AppState) -> DiscoveryTarget {
    if let Some(target) = focused_target(state) {
        return match target {
            DiscoveryTarget::Local { cwd, binary_path } => DiscoveryTarget::Local {
                cwd: local_discovery_dir(state, Some(&cwd)),
                binary_path,
            },
            remote => remote,
        };
    }

    DiscoveryTarget::Local {
        cwd: local_discovery_dir(state, None),
        binary_path: None,
    }
}

/// Determine the best directory for mise task discovery.
#[allow(deprecated)]
pub fn discovery_dir(state: &crate::AppState) -> String {
    match discovery_target(state) {
        DiscoveryTarget::Local { cwd, .. } => cwd,
        DiscoveryTarget::Remote { .. } => local_discovery_dir(state, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        best_discovery_dir, classify_task_scope, classify_unresolved_signals,
        clear_find_mise_test_probe, clear_tool_version_cache_for_test,
        clear_tool_version_test_probe, discover_tasks, discover_tasks_for_target, discovery_target,
        discovery_target_for_tab, discovery_target_for_workspace, filter_tasks_for_target,
        find_mise, find_mise_test_probe_call_count, has_mise_config, host_is_local,
        install_find_mise_test_probe, install_tool_version_test_probe, local_discovery_dir_from,
        normalize_mise_binary_path, remote_mise_run_command, remote_mise_shell_command,
        remote_shell_respawn_command, resolve_discovery_target_from_signals, shell_quote,
        ssh_command_with_remote_exec, ssh_destination_host, ssh_destination_index,
        task_buttons_from_tasks, task_shell_command, task_spawn_config, tmux_backing_cwd,
        tool_version_chip_text_for_target, tool_version_test_probe_call_count,
        update_discovery_binary_cache, wait_for_tool_version_chip, DiscoveryTarget,
        DiscoveryTargetFailure, MiseTask, MiseTaskScope, PaneDiscoverySignals, ToolVersionEntry,
        REMOTE_DEFAULT_CWD,
    };
    use crate::pane::TmuxBacking;
    use crate::probe::ProbeSnapshot;
    use crate::tmux::{TmuxPaneInfo, TmuxTarget};
    use crate::{AppState, Tab, TabKind};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("taarof-mise-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_mise_config(dir: &Path) {
        std::fs::write(dir.join(".mise.toml"), "[tools]\nnode = \"22.1.0\"\n").unwrap();
    }

    fn tool_version(tool: &str, version: &str, source: &Path) -> ToolVersionEntry {
        ToolVersionEntry {
            tool: tool.to_string(),
            version: version.to_string(),
            source: Some(source.join(".mise.toml").to_string_lossy().into_owned()),
        }
    }

    #[cfg(unix)]
    fn write_executable_mise_fixture(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(
            path,
            "#!/bin/sh\nprintf '%s' '[{\"name\":\"app:test\",\"description\":\"\",\"hide\":false}]'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn acquire_env_lock() -> PathBuf {
        let lock_dir = std::env::temp_dir().join("taarof-test-env-lock");
        loop {
            match std::fs::create_dir(&lock_dir) {
                Ok(()) => return lock_dir,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to acquire env lock: {err}"),
            }
        }
    }

    struct ScopedEnv {
        lock_dir: PathBuf,
        key: &'static str,
        previous: Option<OsString>,
    }

    impl ScopedEnv {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let lock_dir = acquire_env_lock();
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                lock_dir,
                key,
                previous,
            }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
            let _ = std::fs::remove_dir(&self.lock_dir);
        }
    }

    struct CurrentDirGuard {
        old: PathBuf,
    }

    impl CurrentDirGuard {
        fn set(path: &Path) -> Self {
            let old = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { old }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.old);
        }
    }

    #[test]
    fn test_has_mise_config_walks_parent_dirs() {
        let root = temp_dir("parent-config");
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".mise.toml"), "[tasks.test]\nrun = \"true\"\n").unwrap();

        assert!(has_mise_config(&nested));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_has_mise_config_treats_mise_tasks_as_project_marker() {
        let root = temp_dir("task-dir-marker");
        let nested = root.join("a/b/c");
        let task_dir = root.join(".mise/tasks");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("test"), "#!/bin/sh\ntrue\n").unwrap();

        assert!(has_mise_config(&nested));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_best_discovery_dir_prefers_candidate_with_mise_config() {
        let root = temp_dir("best-dir");
        let fallback = root.join("fallback");
        let project = root.join("project/app");
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            root.join("project/.mise.toml"),
            "[tasks.test]\nrun = \"true\"\n",
        )
        .unwrap();

        let selected = best_discovery_dir(vec![fallback.clone(), project.clone()]);

        assert_eq!(selected, Some(project));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_discovery_stays_scoped_to_target_without_mise_config() {
        let _env_lock = ScopedEnv::set("TAAROF_TEST_LOCAL_DISCOVERY_SCOPE", "1");
        let root = temp_dir("local-discovery-target-scope");
        let target = root.join("target-project");
        let ambient = root.join("taarof-checkout");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&ambient).unwrap();
        write_mise_config(&ambient);
        let _cwd = CurrentDirGuard::set(&ambient);

        assert_eq!(
            local_discovery_dir_from(target.to_str().unwrap()),
            target.to_string_lossy(),
            "task discovery must not jump from the pane's project to taarof's ambient checkout"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_workspace_discovery_prefers_repo_root_over_active_tab_directory() {
        let root = temp_dir("workspace-root-priority");
        let repo = root.join("sample");
        let unrelated = root.join("sample-project");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        write_mise_config(&repo);

        let mut state = AppState::new();
        let ws_id = state.active_workspace;
        let tab_id = state.next_id;
        state.next_id += 1;

        {
            let workspace = state.active_ws_mut().expect("default workspace");
            workspace.repo_root = Some(repo.to_string_lossy().into_owned());
            workspace.tabs.push(Tab {
                id: tab_id,
                name: "Sample Project".into(),
                work_origin: crate::workspace::new_tab_work_origin(),
                kind: TabKind::Terminal,
                panes: Box::new(crate::pane::PaneNode::Stub { pane_id: 7 }),
                focused_pane_id: 7,
                next_pane_id: 8,
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
                discovery_cwd: Some(unrelated.to_string_lossy().into_owned()),
                discovered_actions: Vec::new(),
                task_buttons: Vec::new(),
                tracking_data: None,
            });
            workspace.active_tab = tab_id;
        }

        assert_eq!(
            discovery_target_for_workspace(&state, ws_id),
            Some(DiscoveryTarget::Local {
                cwd: repo.to_string_lossy().into_owned(),
                binary_path: None,
            })
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tmux_backing_cwd_uses_probe_metadata() {
        let mut pane_info = ProbeSnapshot::default();
        pane_info.record_success(TmuxPaneInfo {
            current_command: "claude".into(),
            cwd: "/tmp/user/projects/example/project-workspace".into(),
            pid: 42,
            width: 120,
            height: 40,
            session_id: "$1".into(),
            session_created: 1,
            continuity_id: Some("11".repeat(16)),
        });
        let backing = TmuxBacking {
            session_name: "taarof--sample--t18--0".into(),
            target: TmuxTarget::Local,
            expected_generation: None,
            pane_info,
        };

        assert_eq!(
            tmux_backing_cwd(&backing).as_deref(),
            Some("/tmp/user/projects/example/project-workspace")
        );
    }

    #[test]
    fn test_classify_task_scope_project_root() {
        let discovery_root = Path::new("/tmp/workspace/project");
        let task = MiseTask {
            name: "test".into(),
            description: String::new(),
            hide: false,
            source: Some("/tmp/workspace/project/.mise.toml".into()),
            global: false,
        };

        assert_eq!(
            classify_task_scope(&task, discovery_root),
            MiseTaskScope::ProjectRoot
        );
    }

    #[test]
    fn test_classify_task_scope_parent_dir() {
        let discovery_root = Path::new("/tmp/workspace/project");
        let task = MiseTask {
            name: "test".into(),
            description: String::new(),
            hide: false,
            source: Some("/tmp/workspace/.mise.toml".into()),
            global: false,
        };

        assert_eq!(
            classify_task_scope(&task, discovery_root),
            MiseTaskScope::ParentDir
        );
    }

    #[test]
    fn test_filter_tasks_for_target_excludes_global_by_default() {
        crate::config::install_app_config(&crate::config::AppConfig::default());
        let discovery_root = "/tmp/workspace/project".to_string();
        let filtered = filter_tasks_for_target(
            vec![
                MiseTask {
                    name: "test".into(),
                    description: String::new(),
                    hide: false,
                    source: Some("/tmp/workspace/project/.mise.toml".into()),
                    global: false,
                },
                MiseTask {
                    name: "plan".into(),
                    description: String::new(),
                    hide: false,
                    source: Some("/tmp/user/.config/mise/config.toml".into()),
                    global: true,
                },
            ],
            &DiscoveryTarget::Local {
                cwd: discovery_root,
                binary_path: None,
            },
        );

        let names: Vec<_> = filtered.iter().map(|task| task.name.as_str()).collect();
        assert_eq!(names, vec!["test"]);
    }

    #[test]
    fn test_project_config_pins_task_buttons() {
        let tasks = vec![
            super::MiseTask {
                name: "fmt".into(),
                description: String::new(),
                hide: false,
                source: None,
                global: false,
            },
            super::MiseTask {
                name: "lint".into(),
                description: String::new(),
                hide: false,
                source: None,
                global: false,
            },
            super::MiseTask {
                name: "test".into(),
                description: String::new(),
                hide: false,
                source: None,
                global: false,
            },
        ];
        let project = crate::project_config::ProjectConfig {
            mise: crate::project_config::ProjectMiseConfig {
                pinned_tasks: vec!["lint".into(), "fmt".into()],
            },
            ..Default::default()
        };

        let buttons = task_buttons_from_tasks(&tasks, Some(&project));

        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].label, "lint");
        assert_eq!(buttons[0].task_name, "lint");
        assert_eq!(buttons[1].label, "fmt");
        assert_eq!(buttons[1].task_name, "fmt");
    }

    #[test]
    fn test_tool_version_chip_renders_top_three() {
        let _guard = super::task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = temp_dir("tool-version-top-three");
        write_mise_config(&root);
        install_tool_version_test_probe(Some(vec![
            tool_version("node", "22.1.0", &root),
            tool_version("python", "3.12.4", &root),
            tool_version("rust", "1.81.0", &root),
            tool_version("jq", "1.7.1", &root),
            tool_version("gh", "2.45.0", &root),
        ]));

        let chip = wait_for_tool_version_chip(&DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        });

        assert_eq!(chip.as_deref(), Some("node 22.1 • python 3.12 • rust 1.81"));
        clear_tool_version_test_probe();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tool_version_chip_omitted_when_no_mise() {
        let _guard = super::task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = temp_dir("tool-version-no-mise");
        install_tool_version_test_probe(Some(vec![tool_version("node", "22.1.0", &root)]));

        let chip = wait_for_tool_version_chip(&DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        });

        assert_eq!(chip, None);
        assert_eq!(tool_version_test_probe_call_count(), 0);
        clear_tool_version_test_probe();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tool_version_chip_prioritizes_runtimes() {
        let _guard = super::task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = temp_dir("tool-version-priority");
        write_mise_config(&root);
        install_tool_version_test_probe(Some(vec![
            tool_version("gh", "2.45.0", &root),
            tool_version("jq", "1.7.1", &root),
            tool_version("rust", "1.81.0", &root),
            tool_version("node", "22.1.0", &root),
        ]));

        let chip = wait_for_tool_version_chip(&DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        });

        assert_eq!(chip.as_deref(), Some("node 22.1 • rust 1.81 • gh 2.45"));
        clear_tool_version_test_probe();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tool_version_chip_uses_cache_per_project() {
        let _guard = super::task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = temp_dir("tool-version-cache");
        let nested = root.join("packages/app");
        std::fs::create_dir_all(&nested).unwrap();
        write_mise_config(&root);
        install_tool_version_test_probe(Some(vec![tool_version("node", "22.1.0", &root)]));
        let root_target = DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        };
        let nested_target = DiscoveryTarget::Local {
            cwd: nested.to_string_lossy().into_owned(),
            binary_path: None,
        };

        assert_eq!(
            wait_for_tool_version_chip(&root_target).as_deref(),
            Some("node 22.1")
        );
        assert_eq!(
            wait_for_tool_version_chip(&nested_target).as_deref(),
            Some("node 22.1")
        );
        assert_eq!(tool_version_test_probe_call_count(), 1);
        clear_tool_version_test_probe();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_tool_version_chip_first_call_does_not_block_caller() {
        // Regression guard: the sidebar/API caller must never block on
        // mise discovery. The first call returns None immediately and kicks a
        // background worker; the chip surfaces on a subsequent call.
        let _guard = super::task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = temp_dir("tool-version-async");
        write_mise_config(&root);
        install_tool_version_test_probe(Some(vec![tool_version("node", "22.1.0", &root)]));

        let target = DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        };

        // First call: cold cache → returns None without running the probe.
        assert_eq!(tool_version_chip_text_for_target(&target), None);

        // Worker runs on a background thread; poll until the cache fills.
        assert_eq!(
            wait_for_tool_version_chip(&target).as_deref(),
            Some("node 22.1")
        );
        clear_tool_version_test_probe();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_ssh_command_with_remote_exec_adds_remote_command() {
        let argv = ssh_command_with_remote_exec(
            &[
                "ssh".into(),
                "-p".into(),
                "2222".into(),
                "user@example".into(),
            ],
            "echo hello".into(),
            true,
            &[],
        )
        .unwrap();

        assert_eq!(
            argv,
            vec!["ssh", "-tt", "-p", "2222", "user@example", "echo hello",]
        );
    }

    #[test]
    fn test_ssh_command_with_remote_exec_strips_captured_positional_command() {
        // Regression: ssh_argv captured from /proc/<pid>/cmdline of the pane's
        // shell may include a trailing remote command (e.g. the shell respawn).
        // If we forward it verbatim, SSH joins it with our probe via spaces and
        // `exec "$SHELL"` replaces the process before the probe runs.
        let argv = ssh_command_with_remote_exec(
            &[
                "ssh".into(),
                "-tt".into(),
                "developer@host".into(),
                "cd '/srv/app' && exec \"$SHELL\"".into(),
            ],
            "echo probe".into(),
            false,
            &["-o", "BatchMode=yes"],
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-tt",
                "developer@host",
                "echo probe",
            ]
        );
    }

    #[test]
    fn test_ssh_destination_index_handles_value_flags() {
        let argv: Vec<String> = ["ssh", "-p", "2222", "-i", "/k", "host", "trailing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ssh_destination_index(&argv), Some(5));
    }

    #[test]
    fn test_ssh_destination_index_handles_clustered_and_attached_flags() {
        let argv: Vec<String> = ["ssh", "-tt", "-p2222", "user@host"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ssh_destination_index(&argv), Some(3));
    }

    #[test]
    fn test_remote_mise_shell_command_quotes_arguments() {
        let command = remote_mise_shell_command("/srv/app's", &["run", "--raw", "dev"]);

        assert!(command.contains("cd '/srv/app'\\''s'"));
        assert!(command.contains("exec \"$MISE_BIN\" 'run' '--raw' 'dev'"));
    }

    #[test]
    fn test_remote_mise_run_command_quotes_arguments() {
        let command = remote_mise_run_command("/srv/app's", &["run", "--raw", "dev"]);

        assert!(command.contains("cd '/srv/app'\\''s'"));
        assert!(command.contains("\"$MISE_BIN\" 'run' '--raw' 'dev'"));
        assert!(!command.contains("exec \"$MISE_BIN\""));
    }

    #[test]
    fn test_remote_task_shell_command_resolves_binary_path() {
        let target = DiscoveryTarget::Remote {
            host: "example.com".into(),
            cwd: "/srv/app".into(),
            ssh_argv: vec!["ssh".into(), "example.com".into()],
        };

        let command = task_shell_command(&target, "app:test").unwrap();

        assert!(command.contains("cd '/srv/app'"));
        assert!(command.contains("\"$MISE_BIN\" 'run' '--raw' 'app:test'"));
    }

    #[test]
    fn test_remote_shell_respawn_command_preserves_cwd() {
        let command = remote_shell_respawn_command("/srv/app's");

        assert!(command.contains("cd '/srv/app'\\''s'"));
        assert!(command.contains("exec \"$SHELL\""));
    }

    #[cfg(unix)]
    #[test]
    fn test_find_mise_prefers_mise_bin_env_override() {
        let root = temp_dir("mise-bin-override");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("mise-from-env");
        write_executable_mise_fixture(&mise_bin);

        let _env = ScopedEnv::set("MISE_BIN", &mise_bin);
        let resolved = find_mise().expect("MISE_BIN override should resolve");
        assert_eq!(resolved, mise_bin);

        let target = DiscoveryTarget::Local {
            cwd: cwd.to_string_lossy().into_owned(),
            binary_path: Some(resolved.clone()),
        };

        let (_, argv) = task_spawn_config(&target, "app:test").unwrap();
        assert_eq!(
            argv.first().map(String::as_str),
            Some(mise_bin.to_str().unwrap())
        );

        let command = task_shell_command(&target, "app:test").unwrap();
        assert_eq!(
            command,
            format!(
                "{} run --raw 'app:test'",
                shell_quote(mise_bin.to_str().unwrap())
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_tasks_honors_mise_bin_env_override() {
        let root = temp_dir("discover-with-mise-bin");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("mise-from-env");
        write_executable_mise_fixture(&mise_bin);

        let _env = ScopedEnv::set("MISE_BIN", &mise_bin);
        let tasks = discover_tasks(cwd.to_str().unwrap());

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "app:test");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_local_task_spawn_uses_resolved_binary_path() {
        let root = temp_dir("resolved-binary");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("custom-mise");
        write_executable_mise_fixture(&mise_bin);

        let target = DiscoveryTarget::Local {
            cwd: cwd.to_string_lossy().into_owned(),
            binary_path: Some(mise_bin.clone()),
        };

        let tasks = discover_tasks_for_target(&target);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "app:test");

        let (_, argv) = task_spawn_config(&target, "app:test").unwrap();
        assert_eq!(
            argv.first().map(String::as_str),
            Some(mise_bin.to_str().unwrap())
        );

        let command = task_shell_command(&target, "app:test").unwrap();
        assert_eq!(
            command,
            format!(
                "{} run --raw 'app:test'",
                shell_quote(mise_bin.to_str().unwrap())
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_local_task_spawn_normalizes_relative_binary_path() {
        let root = temp_dir("relative-mise-bin");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("custom-mise");
        let expected_binary = normalize_mise_binary_path(&mise_bin);
        write_executable_mise_fixture(&mise_bin);

        let _env = ScopedEnv::set("MISE_BIN", "./custom-mise");
        let _cwd = CurrentDirGuard::set(&root);
        let expected_binary_string = expected_binary.to_string_lossy().into_owned();

        let target = DiscoveryTarget::Local {
            cwd: cwd.to_string_lossy().into_owned(),
            binary_path: find_mise(),
        };

        let tasks = discover_tasks_for_target(&target);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "app:test");

        let (_, argv) = task_spawn_config(&target, "app:test").unwrap();
        assert_eq!(
            argv.first().map(String::as_str),
            Some(expected_binary_string.as_str())
        );

        let command = task_shell_command(&target, "app:test").unwrap();
        assert_eq!(
            command,
            format!(
                "{} run --raw 'app:test'",
                shell_quote(expected_binary_string.as_str())
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_local_task_spawn_resolves_binary_when_target_cache_is_empty() {
        let root = temp_dir("resolve-empty-target-cache");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("custom-mise");
        write_executable_mise_fixture(&mise_bin);

        let _env = ScopedEnv::set("MISE_BIN", &mise_bin);
        let target = DiscoveryTarget::Local {
            cwd: cwd.to_string_lossy().into_owned(),
            binary_path: None,
        };

        let (_, argv) = task_spawn_config(&target, "app:test").unwrap();
        assert_eq!(
            argv.first().map(String::as_str),
            Some(mise_bin.to_str().unwrap())
        );

        let command = task_shell_command(&target, "app:test").unwrap();
        assert_eq!(
            command,
            format!(
                "{} run --raw 'app:test'",
                shell_quote(mise_bin.to_str().unwrap())
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_local_task_shell_command_uses_resolved_binary_path() {
        let root = temp_dir("resolved-shell-command");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let mise_bin = root.join("custom-mise");
        write_executable_mise_fixture(&mise_bin);

        let target = DiscoveryTarget::Local {
            cwd: cwd.to_string_lossy().into_owned(),
            binary_path: Some(mise_bin.clone()),
        };

        let command = task_shell_command(&target, "app:test").unwrap();
        assert_eq!(
            command,
            format!(
                "{} run --raw 'app:test'",
                shell_quote(mise_bin.to_str().unwrap())
            )
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Test fixture: an `AppState` pre-populated with one or more
    /// terminal tabs whose `discovery_cwd` is set, plus an executable
    /// fake `mise` binary cached in `state.discovery_binary_paths`.
    ///
    /// Replaces the per-test scaffolding that previously duplicated a
    /// 30-line `Tab { ... }` literal across three different
    /// `test_discovery_target_*` cases. Add a tab with
    /// [`Self::add_tab`], then assert behavior through
    /// [`Self::assert_target_uses_cached_mise`].
    #[cfg(unix)]
    struct TestDiscoveryState {
        root: PathBuf,
        state: AppState,
        mise_bin: PathBuf,
        tab_ids: Vec<u32>,
    }

    impl TestDiscoveryState {
        fn new(label: &str) -> Self {
            let root = temp_dir(label);
            let mise_bin = root.join("custom-mise");
            write_executable_mise_fixture(&mise_bin);
            Self {
                root,
                state: AppState::new(),
                mise_bin,
                tab_ids: Vec::new(),
            }
        }

        /// Add a terminal tab with a stubbed pane, optional `cwd`
        /// (relative to the fixture's temp root), and an optional
        /// `name`. Returns the assigned tab id.
        fn add_tab(&mut self, name: &str, pane_id: u32, cwd: Option<&Path>) -> u32 {
            let tab_id = self.state.next_id;
            self.state.next_id += 1;
            let cwd_string = cwd.map(|c| c.to_string_lossy().into_owned());
            let tab = Tab {
                id: tab_id,
                name: name.into(),
                work_origin: crate::workspace::new_tab_work_origin(),
                kind: TabKind::Terminal,
                panes: Box::new(crate::pane::PaneNode::Stub { pane_id }),
                focused_pane_id: pane_id,
                next_pane_id: pane_id + 1,
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
                discovery_cwd: cwd_string,
                discovered_actions: Vec::new(),
                task_buttons: Vec::new(),
                tracking_data: None,
            };
            self.state
                .active_ws_mut()
                .expect("default workspace")
                .tabs
                .push(tab);
            self.tab_ids.push(tab_id);
            tab_id
        }

        /// Mark the given tab as the currently focused tab in its
        /// (currently active) workspace. Must be called from the same
        /// workspace that holds the tab; for cross-workspace tests,
        /// call [`Self::add_workspace`] first.
        fn focus_tab(&mut self, tab_id: u32) {
            self.state
                .active_ws_mut()
                .expect("default workspace")
                .active_tab = tab_id;
        }

        /// Add a second workspace. The active workspace pointer moves
        /// to the new workspace, so any subsequent `focus_tab` calls
        /// target it.
        fn add_workspace(&mut self, name: &str) {
            self.state.create_workspace(name, None);
        }

        /// Cache the fake `mise` binary in the state's
        /// `discovery_binary_paths` map for `tab_id`.
        fn cache_mise(&mut self, tab_id: u32) {
            self.state
                .discovery_binary_paths
                .insert(tab_id, self.mise_bin.clone());
        }

        fn target_for_focused(&self) -> DiscoveryTarget {
            discovery_target(&self.state)
        }

        fn target_for_tab(&self, tab_id: u32) -> Option<DiscoveryTarget> {
            discovery_target_for_tab(&self.state, tab_id)
        }

        /// Build the spawn config + shell command for the given
        /// target and assert both route through the cached fake
        /// `mise` binary. Returns the resolved shell command for any
        /// extra assertions the caller may want.
        fn assert_target_uses_cached_mise(&self, target: &DiscoveryTarget, task: &str) -> String {
            let (_, argv) = task_spawn_config(target, task).unwrap();
            assert_eq!(
                argv.first().map(String::as_str),
                Some(self.mise_bin.to_str().unwrap()),
                "spawn argv should start with cached mise binary",
            );
            let command = task_shell_command(target, task).unwrap();
            assert_eq!(
                command,
                format!(
                    "{} run --raw '{}'",
                    shell_quote(self.mise_bin.to_str().unwrap()),
                    task
                ),
                "shell command should invoke cached mise with --raw",
            );
            command
        }
    }

    impl Drop for TestDiscoveryState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_discovery_target_reuses_cached_binary_path_for_focused_tab() {
        let mut fixture = TestDiscoveryState::new("cached-discovery-path");
        let cwd = fixture.root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let tab_id = fixture.add_tab("Shell", 7, Some(&cwd));
        fixture.focus_tab(tab_id);
        fixture.cache_mise(tab_id);

        let target = fixture.target_for_focused();
        fixture.assert_target_uses_cached_mise(&target, "app:test");
    }

    #[cfg(unix)]
    #[test]
    fn dashboard_presentation_preserves_focused_mise_target() {
        let mut fixture = TestDiscoveryState::new("dashboard-mise-target");
        let cwd = fixture.root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let tab_id = fixture.add_tab("Shell", 7, Some(&cwd));
        fixture.focus_tab(tab_id);
        fixture.cache_mise(tab_id);
        let dashboard_id = fixture.state.create_dashboard_tab().expect("dashboard tab");

        assert_eq!(fixture.state.presented_tab_id(), Some(dashboard_id));
        assert_eq!(fixture.state.active_tab().map(|tab| tab.id), Some(tab_id));
        let target = fixture.target_for_focused();
        fixture.assert_target_uses_cached_mise(&target, "app:test");
    }

    #[cfg(unix)]
    #[test]
    fn test_discovery_target_for_tab_reuses_cached_binary_path_for_inactive_tab() {
        let mut fixture = TestDiscoveryState::new("cached-discovery-path-inactive");
        let first_cwd = fixture.root.join("project-a");
        let second_cwd = fixture.root.join("project-b");
        std::fs::create_dir_all(&first_cwd).unwrap();
        std::fs::create_dir_all(&second_cwd).unwrap();
        let first_tab_id = fixture.add_tab("Shell A", 7, Some(&first_cwd));
        let second_tab_id = fixture.add_tab("Shell B", 9, Some(&second_cwd));
        fixture.focus_tab(second_tab_id);
        fixture.cache_mise(first_tab_id);

        let target = fixture
            .target_for_tab(first_tab_id)
            .expect("inactive tab target resolves");
        fixture.assert_target_uses_cached_mise(&target, "app:test");
    }

    #[test]
    fn test_discovery_target_for_tab_returns_none_for_tab_in_inactive_workspace() {
        let mut fixture = TestDiscoveryState::new("cross-workspace-guard");
        let cwd = fixture.root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        // Add a tab to the initial (default) workspace, focus it.
        let tab_id = fixture.add_tab("Shell", 7, Some(&cwd));
        fixture.focus_tab(tab_id);

        // Switch to a second workspace — active_workspace now points away from the tab's workspace.
        fixture.add_workspace("other");
        assert_ne!(
            fixture.state.active_workspace, 0,
            "active workspace should have changed"
        );

        // The tab exists (find_tab would find it), but it lives in the now-inactive workspace.
        assert!(
            fixture.state.find_tab(tab_id).is_some(),
            "find_tab should still locate the tab across workspaces"
        );
        assert!(
            fixture.target_for_tab(tab_id).is_none(),
            "should return None when the tab's workspace is not active"
        );
    }

    /// Regression test: when discovery sees an empty
    /// `binary_path` (i.e. `Local { binary_path: None }`), the discovery
    /// pipeline must resolve `find_mise()` once and stash the result in
    /// `state.discovery_binary_paths`. Execution code that later builds a
    /// `DiscoveryTarget` for the same tab then reads the cached path and
    /// does not re-resolve under a potentially-different `PATH`.
    #[test]
    fn test_update_discovery_binary_cache_caches_resolved_path_for_execution() {
        // Probe is keyed by thread id, so the discovery-side and
        // execution-side resolves both observe a deterministic mise path.
        let fake_mise = PathBuf::from("/tmp/taarof-test-fake-mise");
        install_find_mise_test_probe(Some(fake_mise.clone()), Duration::from_millis(0));

        let mut state = AppState::new();
        let tab_id: u32 = 4242;

        let discovery_target = DiscoveryTarget::Local {
            cwd: "/tmp/taarof-test-project".to_string(),
            binary_path: None, // discovery hasn't pre-resolved a binary
        };

        // Discovery-side: populate the cache.
        update_discovery_binary_cache(&mut state, tab_id, &discovery_target);

        assert_eq!(
            state.discovery_binary_paths.get(&tab_id),
            Some(&fake_mise),
            "discovery should have cached the resolved mise binary path",
        );
        let after_first_resolve = find_mise_test_probe_call_count();
        assert_eq!(
            after_first_resolve, 1,
            "discovery should have called find_mise exactly once",
        );

        // Execution-side: build the same shape of target with binary_path
        // sourced from the cache (mirrors what target_for_tab does).
        let cached_binary = state.discovery_binary_paths.get(&tab_id).cloned();
        let execution_target = DiscoveryTarget::Local {
            cwd: "/tmp/taarof-test-project".to_string(),
            binary_path: cached_binary,
        };
        let (_, argv) =
            task_spawn_config(&execution_target, "build").expect("execution must build a command");

        assert_eq!(
            argv.first().map(String::as_str),
            Some(fake_mise.to_str().unwrap()),
            "execution must spawn the cached binary, not a freshly-resolved one",
        );
        assert_eq!(
            find_mise_test_probe_call_count(),
            after_first_resolve,
            "execution must NOT re-resolve find_mise — cache miss means PATH skew can break tasks",
        );

        // Idempotence: a second discovery pass over the same tab must not
        // re-run find_mise either.
        update_discovery_binary_cache(&mut state, tab_id, &discovery_target);
        assert_eq!(
            find_mise_test_probe_call_count(),
            after_first_resolve,
            "subsequent discovery passes must reuse the cache, not re-resolve",
        );
        assert_eq!(
            state.discovery_binary_paths.get(&tab_id),
            Some(&fake_mise),
            "cache value must remain stable across discovery passes",
        );

        // Switching the same tab to a Remote target must evict the stale
        // local binary so we don't try to spawn `/tmp/fake-mise` over ssh.
        let remote_target = DiscoveryTarget::Remote {
            host: "example.com".into(),
            cwd: "/srv/app".into(),
            ssh_argv: vec!["ssh".into(), "example.com".into()],
        };
        update_discovery_binary_cache(&mut state, tab_id, &remote_target);
        assert!(
            !state.discovery_binary_paths.contains_key(&tab_id),
            "Remote targets must clear the local-binary cache entry",
        );

        clear_find_mise_test_probe();
    }

    // ---- EXAMPLE-33: manual-SSH discovery target resolution ------------------

    /// A hostname that is definitely not this machine, so `host_is_local` is
    /// false regardless of where the suite runs.
    const REMOTE_HOST: &str = "build.example.invalid";

    fn signals(
        cwd: Option<&str>,
        cwd_host: Option<&str>,
        ssh_command: Option<&[&str]>,
        remote_shell: bool,
    ) -> PaneDiscoverySignals {
        PaneDiscoverySignals {
            cwd: cwd.map(str::to_string),
            cwd_host: cwd_host.map(str::to_string),
            ssh_command: ssh_command.map(|argv| argv.iter().map(|s| s.to_string()).collect()),
            remote_shell,
        }
    }

    /// (a) A manual ssh whose remote emitted OSC 7 (host + cwd) resolves to a
    /// remote target at that host and directory. With no recorded ssh argv the
    /// target falls back to a bare `ssh <host>` connection.
    #[test]
    fn discovery_target_for_tab_resolves_manual_ssh_location_metadata() {
        let target = resolve_discovery_target_from_signals(
            &signals(Some("/srv/app"), Some(REMOTE_HOST), None, false),
            None,
            None,
        )
        .expect("non-local OSC 7 host + cwd should resolve to a remote target");
        assert_eq!(
            target,
            DiscoveryTarget::Remote {
                host: REMOTE_HOST.to_string(),
                cwd: "/srv/app".to_string(),
                ssh_argv: vec!["ssh".to_string(), REMOTE_HOST.to_string()],
            }
        );
    }

    /// (a) When a real ssh argv was recorded for the pane it is preferred over the
    /// guessed `ssh <host>` fallback so port/identity options are preserved.
    #[test]
    fn discovery_target_for_tab_prefers_recorded_ssh_argv() {
        let argv = ["ssh", "-p", "2222", "developer@build.example.invalid"];
        let target = resolve_discovery_target_from_signals(
            &signals(Some("/srv/app"), Some(REMOTE_HOST), Some(&argv), true),
            None,
            None,
        )
        .expect("recorded ssh argv should resolve to a remote target");
        match target {
            DiscoveryTarget::Remote { ssh_argv, .. } => assert_eq!(
                ssh_argv,
                vec![
                    "ssh".to_string(),
                    "-p".to_string(),
                    "2222".to_string(),
                    "developer@build.example.invalid".to_string(),
                ],
            ),
            other => panic!("expected remote target, got {other:?}"),
        }
    }

    /// (a) Self-detection guard: a local shell that advertises its own hostname
    /// over OSC 7 (or via its title) must NOT be treated as remote — otherwise an
    /// explicit discovery would trigger a doomed ssh-to-self.
    #[test]
    fn discovery_target_for_tab_does_not_treat_local_hostname_as_remote() {
        let local: String = glib::host_name().into();
        let target = resolve_discovery_target_from_signals(
            &signals(Some("/tmp/user/repo"), Some(&local), None, false),
            None,
            None,
        )
        .expect("local self-host should resolve to a local target");
        assert_eq!(
            target,
            DiscoveryTarget::Local {
                cwd: "/tmp/user/repo".to_string(),
                binary_path: None,
            }
        );
    }

    /// (b) Manual ssh detected by the process probe (remote_shell + ssh argv) but
    /// no reliable remote cwd/host metadata: resolve the destination host from the
    /// ssh argv and probe the remote default directory.
    #[test]
    fn discovery_target_for_tab_falls_back_to_ssh_command_destination() {
        let argv = ["ssh", "-p", "22", "developer@build.example.invalid"];
        let target = resolve_discovery_target_from_signals(
            // cwd here is the *local* pre-ssh directory; it must not be replayed
            // as a remote path.
            &signals(Some("/tmp/user/local"), None, Some(&argv), true),
            None,
            None,
        )
        .expect("remote_shell + ssh argv should resolve to a remote target");
        assert_eq!(
            target,
            DiscoveryTarget::Remote {
                host: REMOTE_HOST.to_string(),
                cwd: REMOTE_DEFAULT_CWD.to_string(),
                ssh_argv: vec![
                    "ssh".to_string(),
                    "-p".to_string(),
                    "22".to_string(),
                    "developer@build.example.invalid".to_string(),
                ],
            }
        );
    }

    /// A taarof-managed / configured SSH pane (host + cwd + recorded argv) keeps
    /// resolving to the same remote target — no regression from the manual-ssh work.
    #[test]
    fn discovery_target_for_tab_keeps_managed_ssh_pane_unchanged() {
        let argv = ["ssh", "build.example.invalid"];
        let target = resolve_discovery_target_from_signals(
            &signals(
                Some("/srv/example/app"),
                Some(REMOTE_HOST),
                Some(&argv),
                true,
            ),
            None,
            None,
        )
        .expect("managed ssh pane should resolve to a remote target");
        assert_eq!(
            target,
            DiscoveryTarget::Remote {
                host: REMOTE_HOST.to_string(),
                cwd: "/srv/example/app".to_string(),
                ssh_argv: vec!["ssh".to_string(), "build.example.invalid".to_string(),],
            }
        );
    }

    /// (c) A plain local pane with only a cwd stays local.
    #[test]
    fn discovery_target_for_tab_keeps_plain_local_pane_local() {
        let binary = PathBuf::from("/usr/bin/mise");
        let target = resolve_discovery_target_from_signals(
            &signals(Some("/tmp/user/project"), None, None, false),
            None,
            Some(binary.clone()),
        )
        .expect("plain local pane should resolve to a local target");
        assert_eq!(
            target,
            DiscoveryTarget::Local {
                cwd: "/tmp/user/project".to_string(),
                binary_path: Some(binary),
            }
        );
    }

    /// A local pane that has not reported a cwd (and no fallback) yields no target,
    /// which the sidebar surfaces as `NoWorkingDirectory`.
    #[test]
    fn discovery_target_for_tab_returns_none_without_any_cwd() {
        let target =
            resolve_discovery_target_from_signals(&signals(None, None, None, false), None, None);
        assert!(target.is_none());
        assert_eq!(
            classify_unresolved_signals(&signals(None, None, None, false)),
            DiscoveryTargetFailure::NoWorkingDirectory,
        );
    }

    /// A pane that looks remote (process probe flagged remote_shell) but has no
    /// parseable ssh destination and no host/cwd yields no target, and the failure
    /// classifier reports unresolved remote context — an explicit, visible reason
    /// rather than a silent no-op.
    #[test]
    fn explicit_discover_surfaces_unresolved_remote_context() {
        // remote_shell is true but the ssh argv is unusable (not an ssh client we
        // can truncate), so no destination host can be parsed.
        let unusable = ["not-ssh", "blah"];
        let target = resolve_discovery_target_from_signals(
            &signals(None, None, Some(&unusable), true),
            None,
            None,
        );
        assert!(
            target.is_none(),
            "an unresolvable remote pane must not silently resolve to a local target"
        );
        assert_eq!(
            classify_unresolved_signals(&signals(None, None, Some(&unusable), true)),
            DiscoveryTargetFailure::UnresolvedRemoteContext,
        );

        // A bare remote_shell flag with nothing else also reads as unresolved
        // remote context, not a missing working directory.
        assert_eq!(
            classify_unresolved_signals(&signals(None, None, None, true)),
            DiscoveryTargetFailure::UnresolvedRemoteContext,
        );
    }

    /// `host_is_local` treats loopback aliases and this machine's hostname as
    /// local, and everything else as remote.
    #[test]
    fn host_is_local_guards_self_and_loopback() {
        assert!(host_is_local("localhost"));
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("::1"));
        assert!(host_is_local(""));
        let local: String = glib::host_name().into();
        assert!(host_is_local(&local));
        // Short-name match: the bare hostname without a domain suffix.
        let short = local.split('.').next().unwrap_or(&local);
        assert!(host_is_local(short));
        assert!(!host_is_local(REMOTE_HOST));
    }

    /// `ssh_destination_host` strips `user@` and skips option/value flags to find
    /// the destination host.
    #[test]
    fn ssh_destination_host_parses_user_port_and_options() {
        let plain: Vec<String> = ["ssh", "devbox"].iter().map(|s| s.to_string()).collect();
        assert_eq!(ssh_destination_host(&plain), Some("devbox".to_string()));

        let with_user: Vec<String> = ["ssh", "developer@devbox"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ssh_destination_host(&with_user), Some("devbox".to_string()));

        let with_port: Vec<String> = ["ssh", "-p", "2222", "developer@devbox"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ssh_destination_host(&with_port), Some("devbox".to_string()));

        let with_options: Vec<String> =
            ["ssh", "-tt", "-o", "BatchMode=yes", "root@server.internal"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(
            ssh_destination_host(&with_options),
            Some("server.internal".to_string())
        );

        let no_destination: Vec<String> = ["ssh", "-p", "2222"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ssh_destination_host(&no_destination), None);
    }
}
