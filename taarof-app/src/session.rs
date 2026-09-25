use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex, OnceLock, Weak,
};

use crate::dashboard::SavedDetachedSession;
use crate::instance;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedAgentSession {
    pub agent_name: String,
    pub session_id: String,
    /// Canonical host that owned this opaque provider identity when captured.
    /// Legacy layouts omit it and cannot authorize Resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_identity: Option<String>,
    pub source: SavedAgentSessionSource,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SavedAgentSessionSource {
    Argv,
    TranscriptRecency,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedTmuxIdentity {
    pub session_id: String,
    pub session_created: u64,
    pub continuity_id: String,
}

/// Persisted pane tree node — mirrors PaneNode but serializable.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // Leaf mirrors the complete persisted pane descriptor.
pub enum SavedPaneNode {
    #[serde(rename = "leaf")]
    Leaf {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        work_origin: Option<String>,
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        ssh_command: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        tmux_session: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        tmux_host: Option<String>,
        /// Exact tmux server/session generation captured by the live metadata
        /// probe. A legacy name alone is display metadata, never reattach
        /// authority.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        tmux_identity: Option<SavedTmuxIdentity>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        current_task: Option<crate::task_binding::PaneTaskBinding>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        agent_session: Option<SavedAgentSession>,
    },
    #[serde(rename = "split")]
    Split {
        direction: String,
        ratio: f64,
        first: Box<SavedPaneNode>,
        second: Box<SavedPaneNode>,
    },
}

impl SavedPaneNode {
    fn set_agent_session_for_work_origin(
        &mut self,
        work_origin: &str,
        agent_session: SavedAgentSession,
    ) -> bool {
        match self {
            SavedPaneNode::Leaf {
                work_origin: leaf_origin,
                agent_session: saved_agent_session,
                ..
            } if leaf_origin.as_deref() == Some(work_origin) => {
                *saved_agent_session = Some(agent_session);
                true
            }
            SavedPaneNode::Split { first, second, .. } => {
                first.set_agent_session_for_work_origin(work_origin, agent_session.clone())
                    || second.set_agent_session_for_work_origin(work_origin, agent_session)
            }
            _ => false,
        }
    }

    pub(crate) fn local_cwd_for_pane(&self, target_pane_id: u32) -> Option<String> {
        fn visit(node: &SavedPaneNode, target: u32, next_id: &mut u32) -> Option<String> {
            match node {
                SavedPaneNode::Leaf {
                    cwd,
                    ssh_command,
                    tmux_host,
                    ..
                } => {
                    let pane_id = *next_id;
                    *next_id += 1;
                    if pane_id != target || ssh_command.is_some() || tmux_host.is_some() {
                        return None;
                    }
                    cwd.as_deref()
                        .map(str::trim)
                        .filter(|cwd| cwd.starts_with('/') && !cwd.is_empty())
                        .map(str::to_string)
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, target, next_id).or_else(|| visit(second, target, next_id))
                }
            }
        }
        visit(self, target_pane_id, &mut 0)
    }

    pub fn work_origin_for_pane(&self, target_pane_id: u32) -> Option<&str> {
        fn visit<'a>(node: &'a SavedPaneNode, target: u32, next_id: &mut u32) -> Option<&'a str> {
            match node {
                SavedPaneNode::Leaf { work_origin, .. } => {
                    let pane_id = *next_id;
                    *next_id += 1;
                    (pane_id == target)
                        .then_some(work_origin.as_deref())
                        .flatten()
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, target, next_id).or_else(|| visit(second, target, next_id))
                }
            }
        }
        visit(self, target_pane_id, &mut 0)
    }

    /// Normalize legacy/corrupt persisted pane origins before eager or lazy
    /// restore. Duplicate leaves receive fresh tokens so one tab can never
    /// merge two pane histories.
    pub(crate) fn normalize_work_origins(&mut self) {
        fn visit(node: &mut SavedPaneNode, seen: &mut HashSet<String>) {
            match node {
                SavedPaneNode::Leaf { work_origin, .. } => {
                    let accepted = work_origin
                        .as_deref()
                        .filter(|value| {
                            crate::workspace::valid_work_origin(value)
                                && seen.insert((*value).to_string())
                        })
                        .map(str::to_string);
                    *work_origin = Some(accepted.unwrap_or_else(|| loop {
                        let generated = crate::pane::new_pane_work_origin();
                        if seen.insert(generated.clone()) {
                            break generated;
                        }
                    }));
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, seen);
                    visit(second, seen);
                }
            }
        }
        visit(self, &mut HashSet::new());
    }

    fn normalize_reporting_tokens(&mut self, seen: &mut HashSet<String>) {
        fn visit(node: &mut SavedPaneNode, seen: &mut HashSet<String>) {
            match node {
                SavedPaneNode::Leaf { current_task, .. } => {
                    let Some(binding) = current_task.as_mut() else {
                        return;
                    };
                    let accepted =
                        crate::task_binding::valid_reporting_token(&binding.reporting_token)
                            && seen.insert(binding.reporting_token.clone());
                    if !accepted {
                        binding.reporting_token = loop {
                            let generated = crate::task_binding::new_reporting_token();
                            if seen.insert(generated.clone()) {
                                break generated;
                            }
                        };
                    }
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, seen);
                    visit(second, seen);
                }
            }
        }
        visit(self, seen);
    }

    pub fn current_task_for_pane(
        &self,
        target_pane_id: u32,
    ) -> Option<&crate::task_binding::PaneTaskBinding> {
        fn visit<'a>(
            node: &'a SavedPaneNode,
            target: u32,
            next_id: &mut u32,
        ) -> Option<&'a crate::task_binding::PaneTaskBinding> {
            match node {
                SavedPaneNode::Leaf { current_task, .. } => {
                    let pane_id = *next_id;
                    *next_id += 1;
                    (pane_id == target)
                        .then_some(current_task.as_ref())
                        .flatten()
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, target, next_id).or_else(|| visit(second, target, next_id))
                }
            }
        }
        visit(self, target_pane_id, &mut 0)
    }

    /// Clear a lazy-restored leaf binding using the same DFS pane-id assignment
    /// that materialization and query-state use.
    pub fn clear_current_task_for_pane(&mut self, target_pane_id: u32) -> bool {
        fn visit(node: &mut SavedPaneNode, target: u32, next_id: &mut u32) -> bool {
            match node {
                SavedPaneNode::Leaf { current_task, .. } => {
                    let pane_id = *next_id;
                    *next_id += 1;
                    pane_id == target && current_task.take().is_some()
                }
                SavedPaneNode::Split { first, second, .. } => {
                    visit(first, target, next_id) || visit(second, target, next_id)
                }
            }
        }

        let mut next_id = 0;
        visit(self, target_pane_id, &mut next_id)
    }
}

/// A saved tab whose live pane tree has not been built/spawned yet.
///
/// Lazy session restore keeps the sidebar and session model fully populated by
/// registering an `Empty`-paned tab for every saved tab, while stashing the
/// original `SavedPaneNode` here so the real VTE tree and shell processes can be
/// materialized on first activation. Mirrors the `headless_panes` side-map on
/// `AppState`: keyed by tab id, cleared on tab removal and session reset.
#[derive(Clone, Debug)]
pub struct PendingTabRestore {
    pub saved: SavedPaneNode,
    pub cwd: Option<String>,
    pub show_restore_legend: bool,
}

/// Persisted state for a single tab.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedTab {
    pub name: String,
    /// Stable work-ledger identity. Legacy sessions and sanitized templates
    /// omit it and receive a fresh token when restored.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub work_origin: Option<String>,
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub panes: Option<SavedPaneNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub discovery_cwd: Option<String>,
}

/// Transient restore hint shown in the UI after session restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredPaneHint {
    pub number: usize,
    pub label: String,
    pub is_ssh: bool,
}

/// Transient per-tab legend shown immediately after restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredTabLegend {
    pub tab_id: u32,
    pub items: Vec<RestoredPaneHint>,
}

/// Persisted workspace — groups tabs with metadata.
#[derive(Serialize, Deserialize, Clone)]
pub struct SavedWorkspace {
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub work_origin: Option<String>,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub is_worktree: bool,
    #[serde(default)]
    pub working_tree_path: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub linked_issue: Option<String>,
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active_tab_index: usize,
    #[serde(default)]
    pub tmux_backed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub host_config_name: Option<String>,
}

/// V2 persisted session state — workspaces instead of flat tabs.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionStateV2 {
    pub version: u32,
    /// Collision-safe persistence namespace, distinct from the user-visible
    /// session label. Legacy layouts omit it and are imported only from an
    /// unambiguous legacy path.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub session_namespace: Option<String>,
    /// Normalized `TAAROF_SESSION` identity. This is persisted independently
    /// of the digest namespace so restore can reject even a hypothetical
    /// digest collision. It is not the renameable sidebar label.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub session_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub session_name: Option<String>,
    pub workspaces: Vec<SavedWorkspace>,
    pub active_workspace_index: usize,
    pub window_width: i32,
    pub window_height: i32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub detached_sessions: Vec<SavedDetachedSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub background_section_collapsed: Option<bool>,
}

/// A provider-history lookup captured alongside a GTK-safe session snapshot.
/// The writer resolves it later, after the `AppState` borrow has been released.
#[derive(Clone)]
pub(crate) struct AgentSessionLookup {
    workspace_id: u32,
    tab_work_origin: String,
    pane_work_origin: String,
    agent: String,
    cwd: String,
}

impl AgentSessionLookup {
    pub(crate) fn new(
        workspace_id: u32,
        tab_work_origin: String,
        pane_work_origin: String,
        agent: String,
        cwd: String,
    ) -> Self {
        Self {
            workspace_id,
            tab_work_origin,
            pane_work_origin,
            agent,
            cwd,
        }
    }
}

/// A complete session snapshot. GTK builds this while it has the main-thread
/// state; the ordered writer serializes it and resolves provider history later.
#[derive(Clone)]
pub(crate) struct SessionCapture {
    state: SessionStateV2,
    agent_lookups: Vec<AgentSessionLookup>,
    /// Captured by GTK with the rest of the snapshot. The worker may scan its
    /// filesystem roots, but must never initialize environment-derived roots.
    agent_session_catalog: Option<Arc<crate::agent_sessions::AgentSessionCatalog>>,
}

impl SessionCapture {
    #[cfg(test)]
    pub(crate) fn new(state: SessionStateV2) -> Self {
        Self {
            state,
            agent_lookups: Vec::new(),
            agent_session_catalog: None,
        }
    }

    pub(crate) fn with_agent_lookups(
        state: SessionStateV2,
        agent_lookups: Vec<AgentSessionLookup>,
        agent_session_catalog: Arc<crate::agent_sessions::AgentSessionCatalog>,
    ) -> Self {
        Self {
            state,
            agent_lookups,
            agent_session_catalog: Some(agent_session_catalog),
        }
    }

    fn serialize_for_write(mut self) -> io::Result<SerializedSession> {
        let mut provider_error = None;
        if !self.agent_lookups.is_empty() {
            let catalog = self.agent_session_catalog.as_ref().ok_or_else(|| {
                io::Error::other("agent session catalog was not captured with the GTK snapshot")
            })?;
            let discovered = catalog.snapshot_blocking(Vec::new());
            if let Some(provider) = discovered.providers.iter().find(|provider| !provider.ok) {
                let detail = provider
                    .error
                    .as_deref()
                    .unwrap_or("unknown provider-history error");
                provider_error = Some(format!(
                    "session persistence could not discover {} history: {detail}",
                    provider.name
                ));
            }
            let discovery = crate::agent_sessions::AgentSessionDiscovery {
                providers: discovered.providers,
                sessions: discovered.sessions,
                remote_hosts: Vec::new(),
            };
            for lookup in &self.agent_lookups {
                let Some(record) = crate::agent_sessions::most_recent_discovered_session(
                    &discovery,
                    &lookup.agent,
                    &lookup.cwd,
                ) else {
                    continue;
                };
                let Some(workspace) = self
                    .state
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == lookup.workspace_id)
                else {
                    continue;
                };
                let Some(tab) = workspace.tabs.iter_mut().find(|tab| {
                    tab.work_origin.as_deref() == Some(lookup.tab_work_origin.as_str())
                }) else {
                    continue;
                };
                if let Some(panes) = tab.panes.as_mut() {
                    panes.set_agent_session_for_work_origin(
                        &lookup.pane_work_origin,
                        SavedAgentSession {
                            agent_name: lookup.agent.clone(),
                            session_id: record.session_id.clone(),
                            host_identity: crate::agent_sessions::canonical_local_host_identity(),
                            source: SavedAgentSessionSource::TranscriptRecency,
                        },
                    );
                }
            }
        }

        serde_json::to_string_pretty(&self.state)
            .map(|json| SerializedSession {
                json,
                provider_error,
            })
            .map_err(|error| io::Error::other(format!("could not serialize session: {error}")))
    }
}

struct SerializedSession {
    json: String,
    provider_error: Option<String>,
}

impl SessionStateV2 {
    pub(crate) fn normalize_work_origins(&mut self) {
        let mut workspace_origins = HashSet::new();
        let mut tab_origins = HashSet::new();
        let mut reporting_tokens = HashSet::new();
        for workspace in &mut self.workspaces {
            let accepted = workspace
                .work_origin
                .as_deref()
                .filter(|value| {
                    crate::workspace::valid_work_origin(value)
                        && workspace_origins.insert((*value).to_string())
                })
                .map(str::to_string);
            workspace.work_origin = Some(accepted.unwrap_or_else(|| loop {
                let generated = crate::workspace::new_workspace_work_origin();
                if workspace_origins.insert(generated.clone()) {
                    break generated;
                }
            }));
            for tab in &mut workspace.tabs {
                let accepted = tab
                    .work_origin
                    .as_deref()
                    .filter(|value| {
                        crate::workspace::valid_work_origin(value)
                            && tab_origins.insert((*value).to_string())
                    })
                    .map(str::to_string);
                tab.work_origin = Some(accepted.unwrap_or_else(|| loop {
                    let generated = crate::workspace::new_tab_work_origin();
                    if tab_origins.insert(generated.clone()) {
                        break generated;
                    }
                }));
                if let Some(panes) = tab.panes.as_mut() {
                    panes.normalize_work_origins();
                    panes.normalize_reporting_tokens(&mut reporting_tokens);
                }
            }
        }
    }
}

/// V1 persisted session state — flat tab list (legacy format).
#[derive(Serialize, Deserialize)]
pub struct SessionState {
    pub tabs: Vec<SavedTab>,
    pub active_index: usize,
    pub window_width: i32,
    pub window_height: i32,
    /// Session name (loaded from v2, None for v1)
    #[serde(skip)]
    pub session_name: Option<String>,
}

fn state_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/share")
        })
        .join("taarof")
}

fn state_file() -> PathBuf {
    // TAAROF_SESSION namespaces persisted state so separate desktop instances do
    // not clobber each other's restore data.
    state_file_for(state_path(), instance::session_name().as_deref())
}

fn state_file_for(root: PathBuf, session: Option<&str>) -> PathBuf {
    match session {
        Some(name) => root.join(format!(
            "session-{}.json",
            instance::session_storage_key_for(name)
        )),
        None => root.join("session.json"),
    }
}

fn legacy_state_file() -> Option<PathBuf> {
    let raw = instance::session_name()?;
    let slug = instance::session_slug()?;
    (raw == slug).then(|| state_path().join(format!("session-{slug}.json")))
}

fn layout_matches_session(state: &SessionStateV2, expected_identity: Option<&str>) -> bool {
    let expected_identity = expected_identity
        .map(str::trim)
        .filter(|identity| !identity.is_empty());
    let expected_namespace = expected_identity.map(instance::session_storage_key_for);
    state.session_namespace.as_deref() == expected_namespace.as_deref()
        && state.session_identity.as_deref() == expected_identity
}

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
static NEXT_RECOVERY_FILE: AtomicU64 = AtomicU64::new(0);

fn next_session_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
}

fn preserve_failed_layout(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    for _ in 0..64 {
        let sequence = NEXT_RECOVERY_FILE.fetch_add(1, Ordering::Relaxed);
        let recovery = path.with_file_name(format!("{file_name}.recovery.{sequence}"));
        match fs::hard_link(path, &recovery) {
            Ok(()) => {
                // The recovery directory entry now owns the original inode.
                // Removing the active name cannot destroy those bytes; if the
                // removal fails, keep both names and still report preservation.
                let _ = fs::remove_file(path);
                return Ok(recovery);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "session recovery path collision",
    ))
}

fn write_atomically(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
    }

    let mut opened = None;
    for _ in 0..32 {
        let candidate = next_session_temp_path(path);
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => {
                opened = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (tmp_path, mut file) = opened
        .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "session temp collision"))?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

enum SessionWriteCommand {
    Wake,
    Flush(mpsc::Sender<io::Result<()>>),
    Shutdown,
}

type SessionWriteFn = dyn Fn(&Path, &str) -> io::Result<()> + Send + Sync;

const SESSION_WRITER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct SessionWriterInner {
    latest: Mutex<Option<SessionCapture>>,
    wake_pending: AtomicBool,
    shutting_down: AtomicBool,
    last_error: Mutex<Option<String>>,
    reported_error: Mutex<Option<String>>,
    #[cfg(test)]
    stats: SessionWriterStats,
}

struct SessionWriterOwner {
    shared: Arc<SessionWriterInner>,
    tx: Mutex<Option<mpsc::Sender<SessionWriteCommand>>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    worker_thread: std::thread::ThreadId,
    // Per writer rather than a cfg(test) constant: a test that observes the
    // timeout needs a short deadline, while a test that blocks the worker
    // deliberately needs a generous one. One shared short value made the
    // latter race its own synchronization on a loaded machine.
    shutdown_timeout: std::time::Duration,
}

#[cfg(test)]
#[derive(Default)]
struct SessionWriterStats {
    snapshot_requests: std::sync::atomic::AtomicUsize,
    serializations: std::sync::atomic::AtomicUsize,
    writes: std::sync::atomic::AtomicUsize,
    last_serialization_thread: Mutex<Option<std::thread::ThreadId>>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
struct SessionWriterInstrumentation {
    snapshot_requests: usize,
    serializations: usize,
    writes: usize,
    last_serialization_thread: Option<std::thread::ThreadId>,
}

impl SessionWriterInner {
    fn record_error(&self, error: impl Into<String>) {
        let error = error.into();
        if let Ok(mut last) = self.last_error.lock() {
            if last.as_deref() == Some(error.as_str()) {
                return;
            }
            eprintln!("taarof: could not persist session: {error}");
            *last = Some(error);
        }
    }

    fn clear_error(&self) {
        if let Ok(mut error) = self.last_error.lock() {
            *error = None;
        }
        if let Ok(mut error) = self.reported_error.lock() {
            *error = None;
        }
    }

    fn record_write_outcome(&self, provider_error: Option<String>, result: &io::Result<()>) {
        if let Err(error) = result {
            self.record_error(error.to_string());
        } else if let Some(error) = provider_error {
            self.record_error(error);
        } else {
            self.clear_error();
        }
    }

    #[cfg(test)]
    fn instrumentation(&self) -> SessionWriterInstrumentation {
        SessionWriterInstrumentation {
            snapshot_requests: self.stats.snapshot_requests.load(Ordering::Relaxed),
            serializations: self.stats.serializations.load(Ordering::Relaxed),
            writes: self.stats.writes.load(Ordering::Relaxed),
            last_serialization_thread: self
                .stats
                .last_serialization_thread
                .lock()
                .ok()
                .and_then(|thread| *thread),
        }
    }
}

/// The one ordered persistence worker for a session namespace/path.
///
/// GTK only captures a [`SessionCapture`] and swaps it into this worker's
/// newest-slot. Serialization, provider-history discovery, and file writes are
/// all done by the worker, which owns only shared data and coalesces later
/// captures over older ones. The owner retains the sender and join handle.
#[derive(Clone)]
pub(crate) struct SessionWriter(Arc<SessionWriterOwner>);

fn drain_latest_session_capture(
    shared: &SessionWriterInner,
    write: &SessionWriteFn,
    path: &Path,
) -> (bool, io::Result<()>) {
    let mut attempted = false;
    let mut outcome = Ok(());
    loop {
        let capture = shared
            .latest
            .lock()
            .ok()
            .and_then(|mut latest| latest.take());
        if let Some(capture) = capture {
            attempted = true;
            #[cfg(test)]
            {
                shared.stats.serializations.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut thread) = shared.stats.last_serialization_thread.lock() {
                    *thread = Some(std::thread::current().id());
                }
            }
            let serialized = capture.serialize_for_write();
            let provider_error = serialized
                .as_ref()
                .ok()
                .and_then(|serialized| serialized.provider_error.clone());
            let result = serialized.and_then(|serialized| write(path, &serialized.json));
            #[cfg(test)]
            if result.is_ok() {
                shared.stats.writes.fetch_add(1, Ordering::Relaxed);
            }
            shared.record_write_outcome(provider_error, &result);
            outcome = result;
        }

        shared.wake_pending.store(false, Ordering::Release);
        let has_latest = shared.latest.lock().is_ok_and(|latest| latest.is_some());
        if !has_latest || shared.wake_pending.swap(true, Ordering::AcqRel) {
            break;
        }
    }
    (attempted, outcome)
}

impl SessionWriter {
    pub(crate) fn start() -> io::Result<Self> {
        Self::start_for_path(state_file())
    }

    pub(crate) fn blocked(reason: String) -> io::Result<Self> {
        Self::blocked_at(state_file(), reason)
    }

    fn blocked_at(path: PathBuf, reason: String) -> io::Result<Self> {
        let write_reason = reason.clone();
        let writer = Self::start_at(
            path,
            Arc::new(move |_, _| Err(io::Error::other(write_reason.clone()))),
            SESSION_WRITER_SHUTDOWN_TIMEOUT,
        )?;
        writer.0.shared.record_error(reason);
        Ok(writer)
    }

    fn start_for_path(path: PathBuf) -> io::Result<Self> {
        static WRITERS: OnceLock<
            Mutex<std::collections::HashMap<PathBuf, Weak<SessionWriterOwner>>>,
        > = OnceLock::new();
        let writers = WRITERS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut writers = writers.lock().expect("session writer registry should hold");
        writers.retain(|_, writer| writer.strong_count() > 0);
        if let Some(writer) = writers.get(&path).and_then(Weak::upgrade) {
            return Ok(Self(writer));
        }
        let writer = Self::start_at(
            path.clone(),
            Arc::new(write_atomically),
            SESSION_WRITER_SHUTDOWN_TIMEOUT,
        )?;
        writers.insert(path, Arc::downgrade(&writer.0));
        Ok(writer)
    }

    fn start_at(
        path: PathBuf,
        write: Arc<SessionWriteFn>,
        shutdown_timeout: std::time::Duration,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(SessionWriterInner {
            latest: Mutex::new(None),
            wake_pending: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            last_error: Mutex::new(None),
            reported_error: Mutex::new(None),
            #[cfg(test)]
            stats: SessionWriterStats::default(),
        });
        let worker = Arc::clone(&shared);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name("taarof-session-writer".to_string())
            .spawn(move || {
                let _ = started_tx.send(std::thread::current().id());
                while let Ok(command) = rx.recv() {
                    match command {
                        SessionWriteCommand::Wake => {
                            let _ = drain_latest_session_capture(&worker, write.as_ref(), &path);
                        }
                        SessionWriteCommand::Flush(done) => {
                            let (attempted, result) =
                                drain_latest_session_capture(&worker, write.as_ref(), &path);
                            let acknowledgement = if attempted {
                                result
                            } else {
                                worker
                                    .last_error
                                    .lock()
                                    .ok()
                                    .and_then(|error| error.clone())
                                    .map_or_else(|| Ok(()), |error| Err(io::Error::other(error)))
                            };
                            let _ = done.send(acknowledgement);
                            if worker.shutting_down.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        SessionWriteCommand::Shutdown => {
                            let _ = drain_latest_session_capture(&worker, write.as_ref(), &path);
                            break;
                        }
                    }
                }
            })?;
        let worker_thread = started_rx
            .recv()
            .map_err(|_| io::Error::other("session writer failed before starting"))?;
        Ok(Self(Arc::new(SessionWriterOwner {
            shared,
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
            worker_thread,
            shutdown_timeout,
        })))
    }

    pub(crate) fn schedule_autosave(&self, capture: SessionCapture) {
        if self.0.shared.shutting_down.load(Ordering::Acquire) {
            return;
        }
        self.replace_latest(capture);
        self.wake();
    }

    pub(crate) fn shutdown(&self, capture: SessionCapture) -> io::Result<()> {
        if self.0.shared.shutting_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.replace_latest(capture);
        let (done_tx, done_rx) = mpsc::channel();
        let sender = self
            .0
            .tx
            .lock()
            .ok()
            .and_then(|tx| tx.as_ref().cloned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped"));
        let sender = match sender {
            Ok(sender) => sender,
            Err(error) => {
                self.0.shared.record_error(error.to_string());
                return Err(error);
            }
        };
        if sender.send(SessionWriteCommand::Flush(done_tx)).is_err() {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped");
            self.0.shared.record_error(error.to_string());
            return Err(error);
        }
        match done_rx.recv_timeout(self.0.shutdown_timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let error =
                    io::Error::new(io::ErrorKind::TimedOut, "session writer shutdown timed out");
                self.0.shared.record_error(error.to_string());
                // A blocked filesystem or provider call must not hold GTK
                // hostage indefinitely. The queued Flush makes the worker exit
                // once it returns; relinquish this join handle on timeout.
                self.detach_worker_after_timeout();
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let error = io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped");
                self.0.shared.record_error(error.to_string());
                Err(error)
            }
        }
    }

    fn replace_latest(&self, capture: SessionCapture) {
        #[cfg(test)]
        self.0
            .shared
            .stats
            .snapshot_requests
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(mut latest) = self.0.shared.latest.lock() {
            *latest = Some(capture);
        }
    }

    fn detach_worker_after_timeout(&self) {
        if let Ok(mut join) = self.0.join.lock() {
            drop(join.take());
        }
    }

    fn wake(&self) {
        if !self.0.shared.wake_pending.swap(true, Ordering::AcqRel)
            && self
                .0
                .tx
                .lock()
                .ok()
                .and_then(|tx| tx.as_ref().cloned())
                .is_none_or(|tx| tx.send(SessionWriteCommand::Wake).is_err())
        {
            self.0.shared.wake_pending.store(false, Ordering::Release);
            self.0
                .shared
                .record_error("session writer stopped before autosave wake");
        }
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        self.0
            .shared
            .last_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub(crate) fn take_unreported_error(&self) -> Option<String> {
        let error = self.last_error()?;
        let mut reported = self.0.shared.reported_error.lock().ok()?;
        if reported.as_deref() == Some(error.as_str()) {
            return None;
        }
        *reported = Some(error.clone());
        Some(error)
    }

    #[cfg(test)]
    fn for_test(
        path: PathBuf,
        write: impl Fn(&Path, &str) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start_at(path, Arc::new(write), SESSION_WRITER_SHUTDOWN_TIMEOUT)
    }

    #[cfg(test)]
    fn for_test_with_shutdown_timeout(
        path: PathBuf,
        shutdown_timeout: std::time::Duration,
        write: impl Fn(&Path, &str) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start_at(path, Arc::new(write), shutdown_timeout)
    }

    #[cfg(test)]
    fn flush(&self) -> io::Result<()> {
        let (done_tx, done_rx) = mpsc::channel();
        let sender = self
            .0
            .tx
            .lock()
            .ok()
            .and_then(|tx| tx.as_ref().cloned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped"));
        let sender = match sender {
            Ok(sender) => sender,
            Err(error) => {
                self.0.shared.record_error(error.to_string());
                return Err(error);
            }
        };
        if sender.send(SessionWriteCommand::Flush(done_tx)).is_err() {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped");
            self.0.shared.record_error(error.to_string());
            return Err(error);
        }
        done_rx.recv().map_err(|_| {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "session writer stopped");
            self.0.shared.record_error(error.to_string());
            error
        })?
    }

    #[cfg(test)]
    fn is_shutting_down(&self) -> bool {
        self.0.shared.shutting_down.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn instrumentation(&self) -> SessionWriterInstrumentation {
        self.0.shared.instrumentation()
    }
}

impl Drop for SessionWriterOwner {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.get_mut().ok().and_then(Option::take) {
            let _ = tx.send(SessionWriteCommand::Shutdown);
        }
        if let Some(join) = self.join.get_mut().ok().and_then(Option::take) {
            if std::thread::current().id() == self.worker_thread {
                eprintln!(
                    "taarof: session writer owner dropped on its worker thread; not self-joining"
                );
                std::mem::forget(join);
            } else if join.join().is_err() {
                eprintln!("taarof: session writer thread panicked during shutdown");
            }
        }
    }
}

/// Serialize session state to pretty JSON. Cheap and main-thread safe; the
/// heavy file IO is done separately by [`write_serialized_v2`] so autosave can
/// serialize on the main thread and write off-thread.
pub fn serialize_v2(state: &SessionStateV2) -> Option<String> {
    // Stamp the namespace at the persistence boundary as well as in the
    // runtime snapshot builder. This keeps direct save callers and migrated
    // fixtures from producing a file that the same named session must reject
    // on its next load.
    let mut state = state.clone();
    state.session_namespace = instance::session_storage_key();
    state.session_identity = instance::session_name();
    match serde_json::to_string_pretty(&state) {
        Ok(json) => Some(json),
        Err(e) => {
            eprintln!("taarof: could not serialize session: {e}");
            None
        }
    }
}

/// Write already-serialized session JSON to disk atomically (tmp file +
/// rename). Only reads process env for the target path, so it is safe to call
/// from a worker thread via `gio::spawn_blocking`.
pub fn write_serialized_v2(json: &str) {
    if let Err(e) = std::fs::create_dir_all(state_path()) {
        eprintln!("taarof: could not create state dir: {e}");
        return;
    }
    if let Err(e) = write_atomically(&state_file(), json) {
        eprintln!("taarof: could not replace session atomically: {e}");
    }
}

/// Save session state to disk (v2 format with workspaces), synchronously.
/// Used by the shutdown path; the periodic autosave uses the async split.
pub fn save_v2(state: &SessionStateV2) {
    if let Some(json) = serialize_v2(state) {
        write_serialized_v2(&json);
    }
}

/// Load session state from disk as v2 format.
/// Tries v2 first, falls back to v1 (wrapped in a single "default" workspace).
pub fn load_v2() -> Option<SessionStateV2> {
    let path = state_file();
    if let Ok(content) = std::fs::read_to_string(path) {
        let state = load_v2_from_str(&content)?;
        let expected_identity = instance::session_name();
        if !layout_matches_session(&state, expected_identity.as_deref()) {
            eprintln!("taarof: refusing session layout from a mismatched session identity");
            return None;
        }
        return Some(state);
    }
    // Legacy slug-only files are imported only when the raw name was already
    // path-safe and therefore attribution is unambiguous. Colliding names are
    // never guessed across namespaces.
    let legacy = legacy_state_file()?;
    let content = std::fs::read_to_string(legacy).ok()?;
    let mut state = load_v2_from_str(&content)?;
    let expected_identity = instance::session_name();
    if state
        .session_identity
        .as_deref()
        .is_some_and(|identity| Some(identity) != expected_identity.as_deref())
        || state.session_namespace.is_some()
    {
        eprintln!("taarof: refusing ambiguous legacy session layout");
        return None;
    }
    state.session_namespace = instance::session_storage_key();
    state.session_identity = expected_identity;
    Some(state)
}

pub struct StartupSessionLoad {
    pub state: Option<SessionStateV2>,
    pub diagnostic: Option<String>,
    pub writer_block_reason: Option<String>,
}

enum StartupCandidateFailure {
    Preserve(io::Error),
    LeaveInPlace(&'static str),
}

impl StartupCandidateFailure {
    fn reason(&self) -> String {
        match self {
            Self::Preserve(error) => error.to_string(),
            Self::LeaveInPlace(reason) => (*reason).to_string(),
        }
    }
}

fn load_primary_startup_candidate(
    path: &Path,
    expected_identity: Option<&str>,
) -> Result<Option<SessionStateV2>, StartupCandidateFailure> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StartupCandidateFailure::Preserve(error)),
    };
    let state = load_v2_from_str(&content).ok_or_else(|| {
        StartupCandidateFailure::Preserve(io::Error::new(
            io::ErrorKind::InvalidData,
            "layout is corrupt",
        ))
    })?;
    if !layout_matches_session(&state, expected_identity) {
        return Err(StartupCandidateFailure::LeaveInPlace(
            "layout belongs to another session identity",
        ));
    }
    Ok(Some(state))
}

fn load_legacy_startup_candidate(
    path: &Path,
    expected_identity: Option<&str>,
) -> Result<Option<SessionStateV2>, StartupCandidateFailure> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StartupCandidateFailure::Preserve(error)),
    };
    let mut state = load_v2_from_str(&content).ok_or_else(|| {
        StartupCandidateFailure::Preserve(io::Error::new(
            io::ErrorKind::InvalidData,
            "legacy layout is corrupt",
        ))
    })?;
    if state.session_namespace.is_some()
        || state
            .session_identity
            .as_deref()
            .is_some_and(|identity| Some(identity) != expected_identity)
    {
        return Err(StartupCandidateFailure::LeaveInPlace(
            "legacy layout identity is ambiguous",
        ));
    }
    state.session_namespace = expected_identity.map(instance::session_storage_key_for);
    state.session_identity = expected_identity.map(str::to_string);
    Ok(Some(state))
}

fn load_v2_for_startup_at(
    path: &Path,
    legacy: Option<&Path>,
    expected_identity: Option<&str>,
    preserve: &dyn Fn(&Path) -> io::Result<PathBuf>,
) -> StartupSessionLoad {
    let (failed_path, failure) = match load_primary_startup_candidate(path, expected_identity) {
        Ok(Some(state)) => {
            return StartupSessionLoad {
                state: Some(state),
                diagnostic: None,
                writer_block_reason: None,
            };
        }
        Ok(None) => match legacy {
            Some(legacy_path) => {
                match load_legacy_startup_candidate(legacy_path, expected_identity) {
                    Ok(Some(state)) => {
                        return StartupSessionLoad {
                            state: Some(state),
                            diagnostic: None,
                            writer_block_reason: None,
                        };
                    }
                    Ok(None) => {
                        return StartupSessionLoad {
                            state: None,
                            diagnostic: None,
                            writer_block_reason: None,
                        };
                    }
                    Err(error) => (legacy_path, error),
                }
            }
            None => {
                return StartupSessionLoad {
                    state: None,
                    diagnostic: None,
                    writer_block_reason: None,
                };
            }
        },
        Err(error) => (path, error),
    };

    let failure_reason = failure.reason();
    if matches!(failure, StartupCandidateFailure::LeaveInPlace(_)) {
        let reason = format!(
            "saved workspace layout at {} was not opened ({failure_reason}); the original was left unchanged and session persistence is blocked",
            failed_path.display(),
        );
        return StartupSessionLoad {
            state: None,
            diagnostic: Some(reason.clone()),
            writer_block_reason: Some(reason),
        };
    }

    match preserve(failed_path) {
        Ok(recovery) => StartupSessionLoad {
            state: None,
            diagnostic: Some(format!(
                "Saved workspace layout could not be opened ({failure_reason}). The original was preserved at {}. Reopen workspace layout started empty; live processes and display checkpoints were not inferred.",
                recovery.display(),
            )),
            writer_block_reason: None,
        },
        Err(error) => {
            let reason = format!(
                "saved workspace layout could not be opened at {} ({failure_reason}) or preserved: {error}; session persistence is blocked",
                failed_path.display(),
            );
            StartupSessionLoad {
                state: None,
                diagnostic: Some(reason.clone()),
                writer_block_reason: Some(reason),
            }
        }
    }
}

/// Load the desktop layout without ever letting an unreadable or malformed
/// source be replaced by the default empty session. Recoverable failures move
/// the original inode under a collision-safe recovery name. If preservation
/// itself fails, persistence remains blocked for the whole process lifetime.
pub fn load_v2_for_startup() -> StartupSessionLoad {
    let path = state_file();
    let legacy = legacy_state_file();
    let expected_identity = instance::session_name();
    load_v2_for_startup_at(
        &path,
        legacy.as_deref(),
        expected_identity.as_deref(),
        &preserve_failed_layout,
    )
}

fn load_v2_from_str(content: &str) -> Option<SessionStateV2> {
    // Try v2 first (has "version" and "workspaces" keys)
    if let Ok(mut v2) = serde_json::from_str::<SessionStateV2>(content) {
        v2.normalize_work_origins();
        return Some(v2);
    }

    // Fall back to v1 — wrap in a single "default" workspace
    let v1: SessionState = serde_json::from_str(content).ok()?;
    let mut v2 = SessionStateV2 {
        version: 2,
        session_namespace: None,
        session_identity: None,
        session_name: v1.session_name,
        workspaces: vec![SavedWorkspace {
            id: 1,
            work_origin: None,
            name: "default".to_string(),
            collapsed: false,
            repo_root: None,
            is_worktree: false,
            working_tree_path: None,
            branch_name: None,
            linked_issue: None,
            tabs: v1.tabs,
            active_tab_index: v1.active_index,
            tmux_backed: false,
            host_config_name: None,
        }],
        active_workspace_index: 0,
        window_width: v1.window_width,
        window_height: v1.window_height,
        detached_sessions: Vec::new(),
        background_section_collapsed: None,
    };
    v2.normalize_work_origins();
    Some(v2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovery_test_dir(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "taarof-recovery-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn named_session_layout_paths_do_not_cross_slug_collisions() {
        let root = PathBuf::from("/tmp/taarof-session-test");
        let slash = state_file_for(root.clone(), Some("dev/tools"));
        let space = state_file_for(root.clone(), Some("dev tools"));
        assert_ne!(slash, space);
        assert!(slash
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("session-dev-tools-"));
        assert_ne!(
            state_file_for(root.clone(), Some("a?={$}%[)^,b")),
            state_file_for(root, Some(r"a#\,|~^@:?{b"))
        );
    }

    #[test]
    fn named_layout_validates_digest_namespace_and_normalized_raw_identity() {
        let mut state = load_v2_from_str(
            r#"{
                "version": 2,
                "session_namespace": "dev-tools-2b0da25ba5e276b5da41fc8c4379a988",
                "session_identity": "dev/tools",
                "workspaces": [],
                "active_workspace_index": 0,
                "window_width": 1200,
                "window_height": 800
            }"#,
        )
        .unwrap();
        assert!(layout_matches_session(&state, Some("  dev/tools  ")));
        state.session_identity = Some("dev tools".into());
        assert!(!layout_matches_session(&state, Some("dev/tools")));
        state.session_identity = Some("dev/tools".into());
        state.session_namespace = Some("dev-tools-d291da87197bed46f8411bfd530ae34d".into());
        assert!(!layout_matches_session(&state, Some("dev/tools")));
    }

    #[test]
    fn test_load_old_session_format() {
        let json = r#"{"tabs":[{"name":"Shell","cwd":"/tmp/user"}],"active_index":0,"window_width":1200,"window_height":800}"#;
        let session: SessionState = serde_json::from_str(json).unwrap();
        assert_eq!(session.tabs[0].name, "Shell");
        assert_eq!(session.tabs[0].cwd, Some("/tmp/user".to_string()));
        assert!(session.tabs[0].panes.is_none());
    }

    #[test]
    fn test_load_new_session_format() {
        let json = r#"{"tabs":[{"name":"Shell","cwd":"/home","panes":{"type":"split","direction":"vertical","ratio":0.5,"first":{"type":"leaf","cwd":"/tmp/user"},"second":{"type":"leaf","cwd":"/tmp/user"}}}],"active_index":0,"window_width":1200,"window_height":800}"#;
        let session: SessionState = serde_json::from_str(json).unwrap();
        assert!(session.tabs[0].panes.is_some());
        match session.tabs[0].panes.as_ref().unwrap() {
            SavedPaneNode::Split {
                direction, ratio, ..
            } => {
                assert_eq!(direction, "vertical");
                assert!((ratio - 0.5).abs() < 0.01);
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn test_roundtrip_saved_pane_node() {
        let node = SavedPaneNode::Split {
            direction: "horizontal".into(),
            ratio: 0.3,
            first: Box::new(SavedPaneNode::Leaf {
                work_origin: Some("pane-roundtrip-first".into()),
                cwd: Some("/tmp/user".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            }),
            second: Box::new(SavedPaneNode::Leaf {
                work_origin: Some("pane-roundtrip-second".into()),
                cwd: None,
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            }),
        };
        let json = serde_json::to_string(&node).unwrap();
        let restored: SavedPaneNode = serde_json::from_str(&json).unwrap();
        match restored {
            SavedPaneNode::Split {
                direction, ratio, ..
            } => {
                assert_eq!(direction, "horizontal");
                assert!((ratio - 0.3).abs() < 0.01);
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn test_write_atomically_replaces_target() {
        let dir = std::env::temp_dir().join(format!("taarof-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        write_atomically(&path, "first").unwrap();
        write_atomically(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn corrupt_startup_layout_is_preserved_before_an_empty_save() {
        let dir = recovery_test_dir("corrupt");
        let path = dir.join("session.json");
        let original = b"{not valid session json\n";
        fs::write(&path, original).unwrap();

        let loaded = load_v2_for_startup_at(&path, None, None, &preserve_failed_layout);
        assert!(loaded.state.is_none());
        assert!(loaded.writer_block_reason.is_none());
        assert!(loaded
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("preserved at")));
        assert!(!path.exists());

        let recovery = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|candidate| candidate.to_string_lossy().contains(".recovery."))
            .expect("corrupt source must have a recovery entry");
        assert_eq!(fs::read(&recovery).unwrap(), original);

        write_atomically(&path, r#"{"version":2,"workspaces":[]}"#).unwrap();
        assert_eq!(fs::read(&recovery).unwrap(), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn foreign_layout_stays_in_place_and_blocks_all_writes() {
        use std::cell::Cell;

        let dir = recovery_test_dir("foreign");
        let path = dir.join("session.json");
        let foreign = SessionStateV2 {
            version: 2,
            session_namespace: Some(instance::session_storage_key_for("other-session")),
            session_identity: Some("other-session".into()),
            session_name: Some("other-session".into()),
            workspaces: Vec::new(),
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: Vec::new(),
            background_section_collapsed: None,
        };
        let original = serde_json::to_vec_pretty(&foreign).unwrap();
        fs::write(&path, &original).unwrap();
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let preserve_called = Cell::new(false);
        let preserve = |_path: &Path| -> io::Result<PathBuf> {
            preserve_called.set(true);
            Err(io::Error::other("foreign layout must not be relocated"))
        };

        let loaded = load_v2_for_startup_at(&path, None, Some("expected-session"), &preserve);
        assert!(loaded.state.is_none());
        assert!(!preserve_called.get());
        let reason = loaded
            .writer_block_reason
            .expect("foreign layout must block persistence");
        assert!(reason.contains("belongs to another session identity"));
        assert!(reason.contains("left unchanged"));
        let writer = SessionWriter::blocked_at(path.clone(), reason).unwrap();
        writer.schedule_autosave(session_writer_capture("must not overwrite"));
        assert!(writer.flush().is_err());
        assert!(writer
            .shutdown(session_writer_capture("must not overwrite on shutdown"))
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            original_modified
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ambiguous_legacy_layout_stays_in_place_and_blocks_primary_writer() {
        use std::cell::Cell;

        let dir = recovery_test_dir("ambiguous-legacy");
        let primary = dir.join("session.json");
        let legacy = dir.join("session-legacy.json");
        let ambiguous = SessionStateV2 {
            version: 2,
            session_namespace: Some("already-namespaced".into()),
            session_identity: Some("other-session".into()),
            session_name: Some("other-session".into()),
            workspaces: Vec::new(),
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: Vec::new(),
            background_section_collapsed: None,
        };
        let original = serde_json::to_vec_pretty(&ambiguous).unwrap();
        fs::write(&legacy, &original).unwrap();
        let original_modified = fs::metadata(&legacy).unwrap().modified().unwrap();
        let preserve_called = Cell::new(false);
        let preserve = |_path: &Path| -> io::Result<PathBuf> {
            preserve_called.set(true);
            Err(io::Error::other("ambiguous legacy layout must stay put"))
        };

        let loaded =
            load_v2_for_startup_at(&primary, Some(&legacy), Some("expected-session"), &preserve);
        assert!(!preserve_called.get());
        let reason = loaded
            .writer_block_reason
            .expect("ambiguous legacy layout must block persistence");
        assert!(reason.contains("legacy layout identity is ambiguous"));
        let writer = SessionWriter::blocked_at(primary.clone(), reason).unwrap();
        writer.schedule_autosave(session_writer_capture("must not write primary"));
        assert!(writer.flush().is_err());
        assert!(writer
            .shutdown(session_writer_capture("must not write on shutdown"))
            .is_err());
        assert!(!primary.exists());
        assert_eq!(fs::read(&legacy).unwrap(), original);
        assert_eq!(
            fs::metadata(&legacy).unwrap().modified().unwrap(),
            original_modified
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_preservation_blocks_autosave_and_shutdown() {
        let dir = recovery_test_dir("blocked");
        let path = dir.join("session.json");
        let original = b"unreadable fixture";
        fs::write(&path, original).unwrap();
        let deny = |_path: &Path| -> io::Result<PathBuf> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected preservation denial",
            ))
        };

        let loaded = load_v2_for_startup_at(&path, None, None, &deny);
        let reason = loaded
            .writer_block_reason
            .expect("failed preservation must block persistence");
        assert!(reason.contains("injected preservation denial"));
        let writer = SessionWriter::blocked_at(path.clone(), reason).unwrap();
        writer.schedule_autosave(session_writer_capture("must not overwrite"));
        assert!(writer.flush().is_err());
        assert!(writer
            .shutdown(session_writer_capture("must not overwrite on shutdown"))
            .is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_valid_layout_is_loaded_without_recovery() {
        let dir = recovery_test_dir("empty");
        let path = dir.join("session.json");
        fs::write(
            &path,
            r#"{"version":2,"workspaces":[],"active_workspace_index":0,"window_width":1200,"window_height":800}"#,
        )
        .unwrap();
        let loaded = load_v2_for_startup_at(&path, None, None, &preserve_failed_layout);
        assert_eq!(loaded.state.unwrap().workspaces.len(), 0);
        assert!(loaded.diagnostic.is_none());
        assert!(loaded.writer_block_reason.is_none());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_legacy_layout_is_preserved_when_primary_is_absent() {
        let dir = recovery_test_dir("legacy");
        let primary = dir.join("session.json");
        let legacy = dir.join("session-legacy.json");
        fs::write(&legacy, "legacy corrupt bytes").unwrap();
        let loaded = load_v2_for_startup_at(
            &primary,
            Some(&legacy),
            Some("legacy"),
            &preserve_failed_layout,
        );
        assert!(loaded.state.is_none());
        assert!(loaded.writer_block_reason.is_none());
        assert!(!legacy.exists());
        assert!(fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .any(|candidate| candidate.to_string_lossy().contains(".recovery.")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_load_v2_session_format() {
        let json = r#"{"version":2,"workspaces":[{"id":1,"name":"default","tabs":[{"name":"Shell","cwd":"/tmp/user"}],"active_tab_index":0}],"active_workspace_index":0,"window_width":1200,"window_height":800}"#;
        let v2 = load_v2_from_str(json).unwrap();
        assert_eq!(v2.workspaces.len(), 1);
        assert_eq!(v2.workspaces[0].name, "default");
        assert_eq!(v2.workspaces[0].tabs[0].name, "Shell");
        assert_eq!(v2.active_workspace_index, 0);
    }

    #[test]
    fn test_load_v2_with_multiple_workspaces() {
        let json = r#"{"version":2,"workspaces":[{"id":1,"name":"work","tabs":[{"name":"Shell 1","cwd":"/tmp/user"}],"active_tab_index":0},{"id":2,"name":"personal","tabs":[{"name":"Shell 2","cwd":"/tmp/user"}],"active_tab_index":0}],"active_workspace_index":1,"window_width":1200,"window_height":800}"#;
        let v2 = load_v2_from_str(json).unwrap();
        assert_eq!(v2.workspaces.len(), 2);
        assert_eq!(v2.workspaces[0].name, "work");
        assert_eq!(v2.workspaces[1].name, "personal");
        assert_eq!(v2.workspaces[1].tabs[0].name, "Shell 2");
        assert_eq!(v2.active_workspace_index, 1);
    }

    #[test]
    fn test_v1_fallback_wraps_in_workspace() {
        let json = r#"{"tabs":[{"name":"Old Shell","cwd":"/tmp"}],"active_index":0,"window_width":800,"window_height":600}"#;
        let v2 = load_v2_from_str(json).unwrap();
        assert_eq!(v2.workspaces.len(), 1);
        assert_eq!(v2.workspaces[0].name, "default");
        assert_eq!(v2.workspaces[0].tabs[0].name, "Old Shell");
        assert_eq!(v2.workspaces[0].active_tab_index, 0);
        assert_eq!(v2.window_width, 800);
    }

    #[test]
    fn test_v2_preserves_workspace_metadata() {
        let json = r#"{"version":2,"workspaces":[{"id":1,"name":"taarof","collapsed":true,"repo_root":"/tmp/user/taarof","is_worktree":true,"working_tree_path":"/tmp/user/taarof-feature","branch_name":"main","tabs":[{"name":"Shell","cwd":"/tmp/user/taarof"}],"active_tab_index":0}],"active_workspace_index":0,"window_width":1200,"window_height":800}"#;
        let v2 = load_v2_from_str(json).unwrap();
        let ws = &v2.workspaces[0];
        assert_eq!(ws.name, "taarof");
        assert!(ws.collapsed);
        assert_eq!(ws.repo_root.as_deref(), Some("/tmp/user/taarof"));
        assert!(ws.is_worktree);
        assert_eq!(
            ws.working_tree_path.as_deref(),
            Some("/tmp/user/taarof-feature")
        );
        assert_eq!(ws.branch_name.as_deref(), Some("main"));
    }

    #[test]
    fn test_v2_roundtrips_empty_workspaces() {
        let state = SessionStateV2 {
            version: 2,
            session_namespace: None,
            session_identity: None,
            session_name: Some("taarof".into()),
            workspaces: vec![
                SavedWorkspace {
                    id: 1,
                    work_origin: Some("workspace-session-empty".into()),
                    name: "empty".into(),
                    collapsed: false,
                    repo_root: None,
                    is_worktree: false,
                    working_tree_path: None,
                    branch_name: None,
                    linked_issue: None,
                    tabs: Vec::new(),
                    active_tab_index: 0,
                    tmux_backed: false,
                    host_config_name: None,
                },
                SavedWorkspace {
                    id: 2,
                    work_origin: Some("workspace-session-work".into()),
                    name: "work".into(),
                    collapsed: false,
                    repo_root: None,
                    is_worktree: false,
                    working_tree_path: None,
                    branch_name: None,
                    linked_issue: None,
                    tabs: vec![SavedTab {
                        name: "Shell".into(),
                        work_origin: Some("tab-session-test".into()),
                        cwd: Some("/tmp".into()),
                        panes: None,
                        discovery_cwd: None,
                    }],
                    active_tab_index: 0,
                    tmux_backed: false,
                    host_config_name: None,
                },
            ],
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: Vec::new(),
            background_section_collapsed: None,
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored = load_v2_from_str(&json).unwrap();
        assert_eq!(restored.workspaces.len(), 2);
        assert!(restored.workspaces[0].tabs.is_empty());
        assert_eq!(restored.workspaces[1].tabs.len(), 1);
        assert_eq!(restored.active_workspace_index, 0);
    }

    #[test]
    fn test_saved_pane_node_with_tmux_session() {
        let node = SavedPaneNode::Leaf {
            work_origin: Some("pane-tmux-test".into()),
            cwd: Some("/tmp/user".into()),
            ssh_command: None,
            tmux_session: Some("taarof--app--editor--0".into()),
            tmux_host: Some("user@devbox".into()),
            tmux_identity: None,
            current_task: None,
            agent_session: None,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("tmux_session"));
        assert!(json.contains("tmux_host"));

        let restored: SavedPaneNode = serde_json::from_str(&json).unwrap();
        match restored {
            SavedPaneNode::Leaf {
                tmux_session,
                tmux_host,
                ..
            } => {
                assert_eq!(tmux_session.as_deref(), Some("taarof--app--editor--0"));
                assert_eq!(tmux_host.as_deref(), Some("user@devbox"));
            }
            _ => panic!("expected leaf"),
        }
    }

    #[test]
    fn test_saved_pane_node_without_tmux_fields_deserializes() {
        let json = r#"{"type":"leaf","cwd":"/tmp/user"}"#;
        let node: SavedPaneNode = serde_json::from_str(json).unwrap();
        match node {
            SavedPaneNode::Leaf {
                tmux_session,
                tmux_host,
                ..
            } => {
                assert!(tmux_session.is_none());
                assert!(tmux_host.is_none());
            }
            _ => panic!("expected leaf"),
        }
    }

    #[test]
    fn saved_agent_session_roundtrips_and_legacy_leaf_defaults_to_none() {
        let node = SavedPaneNode::Leaf {
            work_origin: None,
            cwd: Some("/tmp/user/project".into()),
            ssh_command: None,
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session: Some(SavedAgentSession {
                agent_name: "codex".into(),
                session_id: "session-123".into(),
                host_identity: Some("build.ts".into()),
                source: SavedAgentSessionSource::TranscriptRecency,
            }),
        };
        let json = serde_json::to_string(&node).unwrap();
        let restored: SavedPaneNode = serde_json::from_str(&json).unwrap();
        match restored {
            SavedPaneNode::Leaf { agent_session, .. } => assert_eq!(
                agent_session,
                Some(SavedAgentSession {
                    agent_name: "codex".into(),
                    session_id: "session-123".into(),
                    host_identity: Some("build.ts".into()),
                    source: SavedAgentSessionSource::TranscriptRecency,
                })
            ),
            _ => panic!("expected leaf"),
        }

        let legacy: SavedPaneNode =
            serde_json::from_str(r#"{"type":"leaf","cwd":"/tmp/user/project"}"#).unwrap();
        match legacy {
            SavedPaneNode::Leaf { agent_session, .. } => assert!(agent_session.is_none()),
            _ => panic!("expected leaf"),
        }
    }

    #[test]
    fn legacy_saved_tab_without_work_origin_deserializes_for_safe_regeneration() {
        let tab: SavedTab = serde_json::from_str(
            r#"{"name":"Legacy","cwd":"/tmp","panes":null,"discovery_cwd":null}"#,
        )
        .unwrap();
        assert!(tab.work_origin.is_none());
    }

    #[test]
    fn session_load_normalizes_missing_invalid_and_duplicate_workspace_and_tab_origins() {
        let json = r#"{
            "version":2,
            "workspaces":[
                {"id":41,"name":"one","work_origin":"workspace-kept","tabs":[
                    {"name":"a","work_origin":"tab-kept","panes":{"type":"leaf","work_origin":"pane-kept","cwd":"/tmp"}}
                ]},
                {"id":77,"name":"two","work_origin":"workspace-kept","tabs":[
                    {"name":"b","work_origin":"tab-kept","panes":{"type":"leaf","work_origin":"bad origin","cwd":"/tmp"}}
                ]},
                {"id":99,"name":"three","work_origin":"bad origin","tabs":[]},
                {"id":123,"name":"legacy","tabs":[]}
            ],
            "active_workspace_index":0,"window_width":100,"window_height":100
        }"#;
        let restored = load_v2_from_str(json).unwrap();
        let workspace_origins = restored
            .workspaces
            .iter()
            .map(|workspace| workspace.work_origin.as_deref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(workspace_origins[0], "workspace-kept");
        assert_eq!(
            workspace_origins
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len(),
            4
        );
        assert!(workspace_origins
            .iter()
            .all(|origin| crate::workspace::valid_work_origin(origin)));
        let first_tab = &restored.workspaces[0].tabs[0];
        let second_tab = &restored.workspaces[1].tabs[0];
        assert_eq!(first_tab.work_origin.as_deref(), Some("tab-kept"));
        assert_ne!(second_tab.work_origin, first_tab.work_origin);
        assert!(crate::workspace::valid_work_origin(
            second_tab.work_origin.as_deref().unwrap()
        ));
        assert_eq!(
            first_tab.panes.as_ref().unwrap().work_origin_for_pane(0),
            Some("pane-kept")
        );
        assert!(crate::workspace::valid_work_origin(
            second_tab
                .panes
                .as_ref()
                .unwrap()
                .work_origin_for_pane(0)
                .unwrap()
        ));
    }

    #[test]
    fn pane_origin_normalization_preserves_valid_and_repairs_legacy_corrupt_duplicates() {
        let mut tree = SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(SavedPaneNode::Leaf {
                work_origin: Some("pane-valid-origin".into()),
                cwd: None,
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            }),
            second: Box::new(SavedPaneNode::Split {
                direction: "horizontal".into(),
                ratio: 0.5,
                first: Box::new(SavedPaneNode::Leaf {
                    work_origin: Some("pane-valid-origin".into()),
                    cwd: None,
                    ssh_command: None,
                    tmux_session: None,
                    tmux_host: None,
                    tmux_identity: None,
                    current_task: None,
                    agent_session: None,
                }),
                second: Box::new(SavedPaneNode::Leaf {
                    work_origin: Some("bad origin".into()),
                    cwd: None,
                    ssh_command: None,
                    tmux_session: None,
                    tmux_host: None,
                    tmux_identity: None,
                    current_task: None,
                    agent_session: None,
                }),
            }),
        };

        tree.normalize_work_origins();
        let origins = (0..3)
            .map(|pane_id| tree.work_origin_for_pane(pane_id).unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(origins[0], "pane-valid-origin");
        assert_ne!(origins[1], origins[0]);
        assert_ne!(origins[2], origins[0]);
        assert_ne!(origins[2], origins[1]);
        assert!(origins.iter().all(|origin| {
            (5..=128).contains(&origin.len())
                && origin
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }));

        let mut legacy: SavedPaneNode =
            serde_json::from_str(r#"{"type":"leaf","cwd":"/tmp"}"#).unwrap();
        assert!(legacy.work_origin_for_pane(0).is_none());
        legacy.normalize_work_origins();
        assert!(legacy.work_origin_for_pane(0).is_some());
    }

    #[test]
    fn test_saved_pane_node_tmux_fields_not_serialized_when_none() {
        let node = SavedPaneNode::Leaf {
            work_origin: None,
            cwd: Some("/tmp/user".into()),
            ssh_command: None,
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session: None,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("tmux_session"));
        assert!(!json.contains("tmux_host"));
    }

    #[test]
    fn test_saved_pane_node_roundtrips_current_task() {
        let node = SavedPaneNode::Leaf {
            work_origin: Some("pane-task-test".into()),
            cwd: Some("/tmp/user/project".into()),
            ssh_command: None,
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: Some(crate::task_binding::PaneTaskBinding {
                task_id: "EXAMPLE-110".into(),
                title: "Bind each pane to its current .plan task".into(),
                checkout_root: Some("/tmp/user/project".into()),
                reporting_token: crate::task_binding::new_reporting_token(),
            }),
            agent_session: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("current_task"));

        let restored: SavedPaneNode = serde_json::from_str(&json).unwrap();
        match restored {
            SavedPaneNode::Leaf { current_task, .. } => {
                let current_task = current_task.expect("current task should roundtrip");
                assert_eq!(current_task.task_id, "EXAMPLE-110");
                assert_eq!(
                    current_task.title,
                    "Bind each pane to its current .plan task"
                );
                assert_eq!(
                    current_task.checkout_root.as_deref(),
                    Some("/tmp/user/project")
                );
            }
            _ => panic!("expected leaf"),
        }
    }

    #[test]
    fn restored_bindings_regenerate_missing_invalid_and_duplicate_reporting_tokens() {
        let duplicate = "ctx-11111111111111111111111111111111".to_string();
        let binding = |token: String| crate::task_binding::PaneTaskBinding {
            task_id: "EXAMPLE-112".into(),
            title: "Report work".into(),
            checkout_root: Some("/repo".into()),
            reporting_token: token,
        };
        let mut tree = SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(SavedPaneNode::Leaf {
                work_origin: None,
                cwd: Some("/repo".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: Some(binding(duplicate.clone())),
                agent_session: None,
            }),
            second: Box::new(SavedPaneNode::Split {
                direction: "horizontal".into(),
                ratio: 0.5,
                first: Box::new(SavedPaneNode::Leaf {
                    work_origin: None,
                    cwd: Some("/repo".into()),
                    ssh_command: None,
                    tmux_session: None,
                    tmux_host: None,
                    tmux_identity: None,
                    current_task: Some(binding(duplicate.clone())),
                    agent_session: None,
                }),
                second: Box::new(SavedPaneNode::Leaf {
                    work_origin: None,
                    cwd: Some("/repo".into()),
                    ssh_command: None,
                    tmux_session: None,
                    tmux_host: None,
                    tmux_identity: None,
                    current_task: Some(binding(String::new())),
                    agent_session: None,
                }),
            }),
        };
        tree.normalize_reporting_tokens(&mut HashSet::new());
        fn tokens(node: &SavedPaneNode, out: &mut Vec<String>) {
            match node {
                SavedPaneNode::Leaf { current_task, .. } => out.push(
                    current_task
                        .as_ref()
                        .expect("bound leaf")
                        .reporting_token
                        .clone(),
                ),
                SavedPaneNode::Split { first, second, .. } => {
                    tokens(first, out);
                    tokens(second, out);
                }
            }
        }
        let mut values = Vec::new();
        tokens(&tree, &mut values);
        assert_eq!(values.len(), 3);
        assert!(values
            .iter()
            .all(|value| crate::task_binding::valid_reporting_token(value)));
        assert_eq!(values.iter().collect::<HashSet<_>>().len(), 3);
        assert_eq!(values[0], duplicate);
    }

    #[test]
    fn test_v2_deserializes_without_detached_sessions() {
        let json = r#"{
            "version": 2,
            "workspaces": [],
            "active_workspace_index": 0,
            "window_width": 1200,
            "window_height": 800
        }"#;
        let state: SessionStateV2 = serde_json::from_str(json).unwrap();
        assert!(state.detached_sessions.is_empty());
        assert_eq!(state.background_section_collapsed, None);
    }

    #[test]
    fn test_v2_roundtrip_with_detached_sessions() {
        use crate::dashboard::SavedDetachedSession;
        let state = SessionStateV2 {
            version: 2,
            session_namespace: None,
            session_identity: None,
            session_name: None,
            workspaces: vec![],
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: vec![SavedDetachedSession {
                session_name: "taarof--default--t3--0".into(),
                host: "localhost".into(),
                workspace: "default".into(),
                ssh_target: None,
                last_command: Some("cargo test".into()),
            }],
            background_section_collapsed: None,
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: SessionStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.detached_sessions.len(), 1);
        assert_eq!(
            restored.detached_sessions[0].session_name,
            "taarof--default--t3--0"
        );
    }

    #[test]
    fn test_v2_roundtrip_with_background_section_collapsed() {
        let state = SessionStateV2 {
            version: 2,
            session_namespace: None,
            session_identity: None,
            session_name: Some("session".into()),
            workspaces: vec![],
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: vec![],
            background_section_collapsed: Some(false),
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        let restored: SessionStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.background_section_collapsed, Some(false));
    }

    fn session_writer_capture(name: &str) -> SessionCapture {
        SessionCapture::new(SessionStateV2 {
            version: 2,
            session_namespace: Some("captured-namespace".into()),
            session_identity: Some("captured identity".into()),
            session_name: Some(name.to_string()),
            workspaces: Vec::new(),
            active_workspace_index: 0,
            window_width: 1200,
            window_height: 800,
            detached_sessions: Vec::new(),
            background_section_collapsed: None,
        })
    }

    #[test]
    fn test_session_writer_older_delayed_autosave_cannot_overwrite_newer_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "taarof-session-writer-older-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);

        let writer = SessionWriter::for_test(path.clone(), move |path, json| {
            if json.contains("\"old\"") {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            write_atomically(path, json)
        })
        .unwrap();

        writer.schedule_autosave(session_writer_capture("old"));
        started_rx.recv().unwrap();
        for snapshot in 0..16 {
            writer.schedule_autosave(session_writer_capture(&format!("burst-{snapshot}")));
        }
        release_tx.send(()).unwrap();
        writer.flush().unwrap();

        let persisted: SessionStateV2 =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.session_name.as_deref(), Some("burst-15"));
        assert_eq!(
            persisted.session_namespace.as_deref(),
            Some("captured-namespace")
        );
        assert_eq!(
            persisted.session_identity.as_deref(),
            Some("captured identity")
        );
        let instrumentation = writer.instrumentation();
        assert_eq!(instrumentation.snapshot_requests, 17);
        assert_eq!(instrumentation.serializations, 2);
        assert_eq!(instrumentation.writes, 2);
        assert_ne!(
            instrumentation.last_serialization_thread,
            Some(std::thread::current().id()),
            "serialization must stay off the caller thread"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_session_writer_shutdown_save_supersedes_inflight_autosave() {
        let dir = std::env::temp_dir().join(format!(
            "taarof-session-writer-shutdown-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);

        // This test asserts ordering, never the timeout, so give it a deadline
        // it cannot reach. The release below waits on a cross-thread handshake,
        // and any wall-clock budget turns that handshake into a race.
        let writer = SessionWriter::for_test_with_shutdown_timeout(
            path.clone(),
            std::time::Duration::from_secs(3600),
            move |path, json| {
                if json.contains("\"autosave\"") {
                    started_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
                write_atomically(path, json)
            },
        )
        .unwrap();

        writer.schedule_autosave(session_writer_capture("autosave"));
        started_rx.recv().unwrap();
        let final_writer = writer.clone();
        let final_save =
            std::thread::spawn(move || final_writer.shutdown(session_writer_capture("shutdown")));
        // Bounded only so a real regression fails instead of hanging. The
        // spawned thread sets this flag in microseconds; a loaded runner that
        // needs longer must not fail the assertion below.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !writer.is_shutting_down() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            writer.is_shutting_down(),
            "final shutdown save should be queued while autosave is in flight"
        );
        release_tx.send(()).unwrap();
        final_save.join().unwrap().unwrap();

        let persisted: SessionStateV2 =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.session_name.as_deref(), Some("shutdown"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_session_writer_temp_paths_cannot_collide() {
        let dir = std::env::temp_dir().join(format!(
            "taarof-session-writer-temp-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let candidate = next_session_temp_path(&path);
        let first = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&candidate);
        assert!(first.is_ok(), "first exclusive temp create should succeed");
        let second = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&candidate)
            .expect_err("exclusive temp create must reject a collision");
        assert_eq!(second.kind(), io::ErrorKind::AlreadyExists);
        assert_ne!(candidate, next_session_temp_path(&path));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_session_writer_registry_returns_one_owner_per_path() {
        let path = std::env::temp_dir().join(format!(
            "taarof-session-writer-registry-{}-{}.json",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        let first = SessionWriter::start_for_path(path.clone()).unwrap();
        let second = SessionWriter::start_for_path(path).unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
        drop(second);
        drop(first);
    }

    #[test]
    fn test_dropped_session_writer_stops_and_joins_worker() {
        struct DropSignal(mpsc::Sender<()>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let (terminated_tx, terminated_rx) = mpsc::channel();
        let sentinel = DropSignal(terminated_tx);
        let writer =
            SessionWriter::for_test(PathBuf::from("/tmp/unused-session.json"), move |_, _| {
                let _ = &sentinel;
                Ok(())
            })
            .unwrap();
        drop(writer);
        terminated_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("dropping the owner must stop and join the worker");
    }

    #[test]
    fn test_session_writer_wake_send_failure_is_observable() {
        let writer =
            SessionWriter::for_test(PathBuf::from("/tmp/unused-session.json"), |_, _| Ok(()))
                .unwrap();
        let sender = writer.0.tx.lock().unwrap().as_ref().unwrap().clone();
        sender.send(SessionWriteCommand::Shutdown).unwrap();
        writer
            .0
            .join
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        writer.wake();
        assert_eq!(
            writer.last_error().as_deref(),
            Some("session writer stopped before autosave wake")
        );
        assert!(writer.take_unreported_error().is_some());
        assert!(writer.take_unreported_error().is_none());
    }

    #[test]
    fn test_session_writer_write_error_reporting_is_deduplicated_and_recovers() {
        let fail_writes = Arc::new(AtomicBool::new(true));
        let fail_writes_for_worker = Arc::clone(&fail_writes);
        let writer = SessionWriter::for_test(
            PathBuf::from("/tmp/unused-session-write-error.json"),
            move |_, _| {
                if fail_writes_for_worker.load(Ordering::Acquire) {
                    Err(io::Error::other("injected write failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();

        writer.schedule_autosave(session_writer_capture("first failure"));
        assert_eq!(
            writer.flush().unwrap_err().to_string(),
            "injected write failure"
        );
        assert_eq!(
            writer.take_unreported_error().as_deref(),
            Some("injected write failure")
        );
        assert!(writer.take_unreported_error().is_none());

        writer.schedule_autosave(session_writer_capture("same failure"));
        assert!(writer.flush().is_err());
        assert!(
            writer.take_unreported_error().is_none(),
            "an unchanged failure must not produce another UI report"
        );

        fail_writes.store(false, Ordering::Release);
        writer.schedule_autosave(session_writer_capture("recovery"));
        writer.flush().unwrap();
        assert!(writer.last_error().is_none());

        fail_writes.store(true, Ordering::Release);
        writer.schedule_autosave(session_writer_capture("failure after recovery"));
        assert!(writer.flush().is_err());
        assert_eq!(
            writer.take_unreported_error().as_deref(),
            Some("injected write failure"),
            "recovery must re-arm reporting for a later failure"
        );
    }

    #[test]
    fn test_session_writer_shutdown_propagates_injected_write_error() {
        let writer = SessionWriter::for_test(
            PathBuf::from("/tmp/unused-session-shutdown-error.json"),
            |_, _| Err(io::Error::other("injected shutdown write failure")),
        )
        .unwrap();

        let error = writer
            .shutdown(session_writer_capture("shutdown failure"))
            .expect_err("the final write error must reach shutdown callers");
        assert_eq!(error.to_string(), "injected shutdown write failure");
        assert_eq!(
            writer.take_unreported_error().as_deref(),
            Some("injected shutdown write failure")
        );
    }

    #[test]
    fn test_agent_lookup_uses_gtk_captured_catalog_on_worker_and_recovers() {
        struct ToggleDiscoveryScanner {
            failing: AtomicBool,
            scan_thread: Mutex<Option<std::thread::ThreadId>>,
        }

        impl crate::agent_sessions::AgentSessionScanner for ToggleDiscoveryScanner {
            fn scan(&self) -> crate::agent_sessions::AgentSessionDiscovery {
                *self.scan_thread.lock().unwrap() = Some(std::thread::current().id());
                let failing = self.failing.load(Ordering::Acquire);
                crate::agent_sessions::AgentSessionDiscovery {
                    providers: vec![crate::agent_sessions::AgentSessionProviderStatus {
                        name: "codex".to_string(),
                        ok: !failing,
                        history_available: !failing,
                        warning: None,
                        error: failing.then(|| "injected discovery failure".to_string()),
                        session_count: 0,
                    }],
                    sessions: Vec::new(),
                    remote_hosts: Vec::new(),
                }
            }
        }

        let scanner = Arc::new(ToggleDiscoveryScanner {
            failing: AtomicBool::new(true),
            scan_thread: Mutex::new(None),
        });
        let catalog = Arc::new(
            crate::agent_sessions::AgentSessionCatalog::with_scanner_and_ttl(
                scanner.clone(),
                std::time::Duration::ZERO,
            ),
        );
        let capture = |name| {
            SessionCapture::with_agent_lookups(
                session_writer_capture(name).state,
                vec![AgentSessionLookup::new(
                    1,
                    "tab".to_string(),
                    "pane".to_string(),
                    "codex".to_string(),
                    "/repo".to_string(),
                )],
                Arc::clone(&catalog),
            )
        };
        let writer = SessionWriter::for_test(
            PathBuf::from("/tmp/unused-session-discovery.json"),
            |_, _| Ok(()),
        )
        .unwrap();

        writer.schedule_autosave(capture("discovery failure"));
        assert!(writer.flush().is_err());
        assert_eq!(
            writer.take_unreported_error().as_deref(),
            Some(
                "session persistence could not discover codex history: injected discovery failure"
            )
        );
        assert_ne!(
            *scanner.scan_thread.lock().unwrap(),
            Some(std::thread::current().id()),
            "provider scanning belongs to the writer, not GTK"
        );

        scanner.failing.store(false, Ordering::Release);
        writer.schedule_autosave(capture("discovery recovery"));
        writer.flush().unwrap();
        assert!(writer.last_error().is_none());
    }

    #[test]
    fn test_session_writer_shutdown_timeout_is_observable_and_does_not_block_caller() {
        let dir = std::env::temp_dir().join(format!(
            "taarof-session-writer-timeout-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (final_tx, final_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        // This test observes the timeout itself, so it asks for a short one.
        let writer = SessionWriter::for_test_with_shutdown_timeout(
            path,
            std::time::Duration::from_millis(100),
            move |_, json| {
                if json.contains("\"autosave\"") {
                    started_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
                if json.contains("\"shutdown\"") {
                    final_tx.send(()).unwrap();
                }
                Ok(())
            },
        )
        .unwrap();
        writer.schedule_autosave(session_writer_capture("autosave"));
        started_rx.recv().unwrap();
        let started = std::time::Instant::now();
        let error = writer
            .shutdown(session_writer_capture("shutdown"))
            .expect_err("blocked worker should time out instead of blocking shutdown");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            writer.last_error().as_deref(),
            Some("session writer shutdown timed out")
        );
        release_tx.send(()).unwrap();
        final_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker should finish its authoritative final snapshot after timeout");
        drop(writer);
        let _ = std::fs::remove_dir_all(dir);
    }
}
