//! WebSocket pane-attach state machine and pane snapshot helpers.

use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex as StdMutex,
};
use tokio::sync::watch;
use tokio::task::AbortHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneAttachKind {
    Tmux {
        session_name: String,
        target: crate::tmux::TmuxTarget,
    },
    Vte,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneAttachTarget {
    pub tab_id: u32,
    pub pane_id: u32,
    pub kind: PaneAttachKind,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct PaneKey {
    pub tab_id: u32,
    pub pane_id: u32,
}

impl From<&PaneAttachTarget> for PaneKey {
    fn from(target: &PaneAttachTarget) -> Self {
        Self {
            tab_id: target.tab_id,
            pane_id: target.pane_id,
        }
    }
}

#[derive(Debug)]
pub struct PaneDirtySignal {
    generation: AtomicU64,
    notify: tokio::sync::Notify,
}

impl PaneDirtySignal {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub fn mark_dirty(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    async fn await_dirty(&self, last_generation: u64) -> u64 {
        loop {
            let notified = self.notify.notified();
            let generation = self.current_generation();
            if generation != last_generation {
                return generation;
            }
            notified.await;
        }
    }
}

impl Default for PaneDirtySignal {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct PaneDirtyRegistry {
    signals: StdMutex<HashMap<PaneKey, Arc<PaneDirtySignal>>>,
}

impl PaneDirtyRegistry {
    pub(crate) fn retain_tabs(&self, live: &HashSet<u32>) {
        self.signals
            .lock()
            .expect("pane dirty registry lock should hold")
            .retain(|key, _| live.contains(&key.tab_id));
    }

    pub fn signal_for(&self, key: PaneKey) -> Arc<PaneDirtySignal> {
        let mut signals = self
            .signals
            .lock()
            .expect("pane dirty registry lock should hold");
        Arc::clone(
            signals
                .entry(key)
                .or_insert_with(|| Arc::new(PaneDirtySignal::new())),
        )
    }

    /// Late VTE callbacks after tab removal must not recreate registry entries.
    /// Before the first subscriber, the initial snapshot supplies current state.
    pub fn mark_dirty(&self, tab_id: u32, pane_id: u32) -> bool {
        let signals = self
            .signals
            .lock()
            .expect("pane dirty registry lock should hold");
        let Some(signal) = signals.get(&PaneKey { tab_id, pane_id }) else {
            return false;
        };
        signal.mark_dirty();
        true
    }

    fn release_unused(&self, key: PaneKey, signal: &Arc<PaneDirtySignal>) {
        let mut signals = self
            .signals
            .lock()
            .expect("pane dirty registry lock should hold");
        if signals
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, signal) && Arc::strong_count(current) == 2)
        {
            signals.remove(&key);
        }
    }
}

// A resolve/capture can fail or be cancelled after allocating its signal.
// Keep cleanup local to that pending subscription; an active source's Arc
// prevents removal, and an old lease cannot delete a replacement registration.
struct PendingDirtyRegistration<'a> {
    registry: &'a PaneDirtyRegistry,
    key: PaneKey,
    signal: &'a Arc<PaneDirtySignal>,
}

impl Drop for PendingDirtyRegistration<'_> {
    fn drop(&mut self) {
        self.registry.release_unused(self.key, self.signal);
    }
}

// Own the registration before spawning: aborting an unpolled task must also
// release its entry. The running source borrows the signal instead of cloning it.
struct SourceDirtyRegistration {
    registry: Arc<PaneDirtyRegistry>,
    key: PaneKey,
    signal: Arc<PaneDirtySignal>,
}

impl Drop for SourceDirtyRegistration {
    fn drop(&mut self) {
        self.registry.release_unused(self.key, &self.signal);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneAttachLookup {
    Attachable(PaneAttachTarget),
    Unsupported,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaneAttachResolveErrorKind {
    Unsupported,
    NotFound,
    LookupFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PaneAttachResolveError {
    kind: PaneAttachResolveErrorKind,
    message: String,
}

impl PaneAttachResolveError {
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: PaneAttachResolveErrorKind::Unsupported,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: PaneAttachResolveErrorKind::NotFound,
            message: message.into(),
        }
    }

    fn lookup_failed(message: impl Into<String>) -> Self {
        Self {
            kind: PaneAttachResolveErrorKind::LookupFailed,
            message: message.into(),
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(self.kind, PaneAttachResolveErrorKind::NotFound)
    }

    fn as_str(&self) -> &str {
        &self.message
    }
}

pub(crate) fn resolve_pane_attach_target(
    state: &crate::AppState,
    pane_id: u32,
) -> PaneAttachLookup {
    resolve_pane_attach_target_for_tab(state, None, pane_id)
}

pub(crate) fn resolve_pane_attach_target_in_tab(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
) -> PaneAttachLookup {
    resolve_pane_attach_target_for_tab(state, Some(tab_id), pane_id)
}

fn resolve_pane_attach_target_for_tab(
    state: &crate::AppState,
    requested_tab_id: Option<u32>,
    pane_id: u32,
) -> PaneAttachLookup {
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if requested_tab_id.is_some_and(|tab_id| tab.id != tab_id) {
                continue;
            }

            if let Some(leaf) = tab.panes.leaf(pane_id) {
                return match leaf.tmux_backing.as_ref() {
                    Some(backing) => PaneAttachLookup::Attachable(PaneAttachTarget {
                        tab_id: tab.id,
                        pane_id,
                        kind: PaneAttachKind::Tmux {
                            session_name: backing.session_name.clone(),
                            target: backing.target.clone(),
                        },
                    }),
                    None => PaneAttachLookup::Attachable(PaneAttachTarget {
                        tab_id: tab.id,
                        pane_id,
                        kind: PaneAttachKind::Vte,
                    }),
                };
            }

            if let Some(headless) = state.headless_pane(tab.id, pane_id) {
                return match headless.tmux_backing.as_ref() {
                    Some(backing) => PaneAttachLookup::Attachable(PaneAttachTarget {
                        tab_id: tab.id,
                        pane_id,
                        kind: PaneAttachKind::Tmux {
                            session_name: backing.session_name.clone(),
                            target: backing.target.clone(),
                        },
                    }),
                    None => PaneAttachLookup::Unsupported,
                };
            }
        }
    }

    PaneAttachLookup::NotFound
}

pub(crate) fn capture_vte_pane_snapshot_sync(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
) -> Result<PaneSnapshot, String> {
    use vte::prelude::*;

    let terminal = {
        let mut found: Option<vte::Terminal> = None;
        'outer: for workspace in &state.workspaces {
            for tab in &workspace.tabs {
                if tab.id != tab_id {
                    continue;
                }
                if let Some(leaf) = tab.panes.leaf(pane_id) {
                    found = Some(leaf.terminal.clone());
                    break 'outer;
                }
            }
        }
        found.ok_or_else(|| format!("pane {pane_id} not found in tab {tab_id}"))?
    };

    let cols = terminal.column_count();
    let row_count = terminal.row_count();
    if cols <= 0 || row_count <= 0 {
        return Ok(PaneSnapshot {
            output: String::new(),
            width: 0,
            height: 0,
        });
    }

    let (_cursor_col, cursor_row) = terminal.cursor_position();
    let visible_start = (cursor_row - row_count + 1).max(0);
    let (text, _len) =
        terminal.text_range_format(vte::Format::Text, visible_start, 0, cursor_row, cols - 1);
    let output = text.map(|g| g.to_string()).unwrap_or_default();
    let snapshot = PaneSnapshot {
        output,
        width: cols as u32,
        height: row_count as u32,
    };
    trace_vte_capture(tab_id, pane_id, visible_start, cursor_row, &snapshot);

    Ok(snapshot)
}

pub(super) async fn query_pane_attach_lookup(
    state: &HttpState,
    tab_id: Option<u32>,
    pane_id: u32,
) -> Result<PaneAttachLookup, StatusCode> {
    query_pane_attach_lookup_from_bridge(&state.bridge, tab_id, pane_id).await
}

async fn query_pane_attach_lookup_from_bridge(
    bridge: &BridgeSender,
    tab_id: Option<u32>,
    pane_id: u32,
) -> Result<PaneAttachLookup, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    bridge
        .try_send(HttpBridgeRequest::ResolvePaneAttach {
            tab_id,
            pane_id,
            reply: reply_tx,
        })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

#[derive(Deserialize, Default)]
pub(super) struct WebSocketAuthQuery {
    pub(super) token: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum PaneControlClientFrame {
    #[serde(rename = "input")]
    Input {
        payload: String,
        #[serde(default)]
        encoding: Option<String>,
    },
    #[serde(rename = "resize")]
    Resize { cols: u32, rows: u32 },
}

const PANE_CONTROL_MAX_COLS: u32 = 1000;
const PANE_CONTROL_MAX_ROWS: u32 = 1000;

pub(super) async fn pane_attach_ws(
    AxumPath(pane_id): AxumPath<u32>,
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(auth): Query<WebSocketAuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    pane_attach_ws_common(None, pane_id, state, headers, auth, ws).await
}

pub(super) async fn tab_pane_attach_ws(
    AxumPath((tab_id, pane_id)): AxumPath<(u32, u32)>,
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(auth): Query<WebSocketAuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    pane_attach_ws_common(Some(tab_id), pane_id, state, headers, auth, ws).await
}

pub(super) async fn pane_attach_ws_common(
    tab_id: Option<u32>,
    pane_id: u32,
    state: HttpState,
    headers: HeaderMap,
    auth: WebSocketAuthQuery,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = check_ws_auth(&headers, auth.token.as_deref(), &state.auth_token) {
        return status.into_response();
    }

    match query_pane_attach_lookup(&state, tab_id, pane_id).await {
        Ok(PaneAttachLookup::Attachable(_target)) => {}
        Ok(PaneAttachLookup::Unsupported) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": "live attach is not supported for this pane type",
                })),
            )
                .into_response();
        }
        Ok(PaneAttachLookup::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": match tab_id {
                        Some(tab_id) => format!("pane {pane_id} was not found in tab {tab_id}"),
                        None => format!("pane {pane_id} was not found"),
                    },
                })),
            )
                .into_response();
        }
        Err(status) => return status.into_response(),
    }

    let attach_permit = match Arc::clone(&state.pane_attach_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "ok": false,
                    "error": "too many live pane attach clients",
                })),
            )
                .into_response();
        }
    };

    ws.on_upgrade(move |socket| {
        handle_pane_attach_socket(socket, state, tab_id, pane_id, attach_permit)
    })
    .into_response()
}

pub(super) async fn tab_pane_attach_control_ws(
    AxumPath((tab_id, pane_id)): AxumPath<(u32, u32)>,
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(auth): Query<WebSocketAuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(status) = check_ws_auth(&headers, auth.token.as_deref(), &state.auth_token) {
        return status.into_response();
    }

    if let Err(status) = check_control_enabled(&state) {
        return status.into_response();
    }

    match query_pane_attach_lookup(&state, Some(tab_id), pane_id).await {
        Ok(PaneAttachLookup::Attachable(_target)) => {}
        Ok(PaneAttachLookup::Unsupported) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": "live control is not supported for this pane type",
                })),
            )
                .into_response();
        }
        Ok(PaneAttachLookup::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": format!("pane {pane_id} was not found in tab {tab_id}"),
                })),
            )
                .into_response();
        }
        Err(status) => return status.into_response(),
    }

    let attach_permit = match Arc::clone(&state.pane_attach_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "ok": false,
                    "error": "too many live pane attach clients",
                })),
            )
                .into_response();
        }
    };

    ws.on_upgrade(move |socket| {
        handle_pane_control_socket(socket, state, tab_id, pane_id, attach_permit)
    })
    .into_response()
}

pub(super) fn default_pane_snapshotter(bridge: BridgeSender) -> Arc<PaneSnapshotter> {
    Arc::new(move |target, preserve_ansi| {
        let bridge = bridge.clone();
        Box::pin(async move {
            match &target.kind {
                PaneAttachKind::Tmux { .. } => capture_tmux_snapshot(target, preserve_ansi).await,
                PaneAttachKind::Vte => capture_vte_snapshot(bridge, target).await,
            }
        })
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PaneAttachBaseline {
    pub(super) target: Option<PaneAttachTarget>,
    pub(super) replace_payload: Option<String>,
    pub(super) size: Option<(u32, u32)>,
}

impl PaneAttachBaseline {
    pub(super) fn from_replace_snapshot(
        target: &PaneAttachTarget,
        snapshot: &PaneSnapshot,
    ) -> Self {
        Self {
            target: Some(target.clone()),
            replace_payload: Some(normalize_snapshot_text(&snapshot.output)),
            size: Some((snapshot.width, snapshot.height)),
        }
    }

    pub(super) fn has_changed(&self, target: &PaneAttachTarget, snapshot: &PaneSnapshot) -> bool {
        let size = (snapshot.width, snapshot.height);
        let normalized_output = normalize_snapshot_text(&snapshot.output);
        self.target.as_ref() != Some(target)
            || self
                .replace_payload
                .as_ref()
                .map(|payload| payload != &normalized_output)
                .unwrap_or(true)
            || self.size != Some(size)
    }

    fn update(&mut self, target: &PaneAttachTarget, snapshot: &PaneSnapshot) {
        self.target = Some(target.clone());
        self.replace_payload = Some(normalize_snapshot_text(&snapshot.output));
        self.size = Some((snapshot.width, snapshot.height));
    }
}

fn normalize_snapshot_text(output: &str) -> String {
    output.replace("\r\n", "\n")
}

async fn resolve_current_pane_attach_target(
    state: &HttpState,
    tab_id: Option<u32>,
    pane_id: u32,
) -> Result<PaneAttachTarget, PaneAttachResolveError> {
    resolve_current_pane_attach_target_from_bridge(&state.bridge, tab_id, pane_id).await
}

async fn resolve_current_pane_attach_target_from_bridge(
    bridge: &BridgeSender,
    tab_id: Option<u32>,
    pane_id: u32,
) -> Result<PaneAttachTarget, PaneAttachResolveError> {
    match query_pane_attach_lookup_from_bridge(bridge, tab_id, pane_id).await {
        Ok(PaneAttachLookup::Attachable(target)) => Ok(target),
        Ok(PaneAttachLookup::Unsupported) => Err(PaneAttachResolveError::unsupported(
            "live attach is not supported for this pane type",
        )),
        Ok(PaneAttachLookup::NotFound) => Err(PaneAttachResolveError::not_found(format!(
            "pane {pane_id} was not found"
        ))),
        Err(status) => Err(PaneAttachResolveError::lookup_failed(format!(
            "pane attach lookup failed with status {}",
            status.as_u16()
        ))),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PaneAttachUpdate {
    Replace {
        target: PaneAttachTarget,
        snapshot: PaneSnapshot,
    },
    Error(String),
    Gone,
}

struct PaneAttachSource {
    subscribers: usize,
    updates: watch::Sender<PaneAttachUpdate>,
    task: AbortHandle,
}

pub(super) struct PaneAttachHub {
    sources: StdMutex<HashMap<PaneKey, PaneAttachSource>>,
    bridge: BridgeSender,
    snapshotter: Arc<PaneSnapshotter>,
    snapshot_slots: Arc<Semaphore>,
    dirty: Arc<PaneDirtyRegistry>,
    poll_interval: Duration,
    liveness_interval: Duration,
    idle_poll_max_interval: Duration,
}

impl PaneAttachHub {
    pub(super) fn new(
        bridge: BridgeSender,
        snapshotter: Arc<PaneSnapshotter>,
        snapshot_slots: Arc<Semaphore>,
        dirty: Arc<PaneDirtyRegistry>,
        poll_interval: Duration,
    ) -> Arc<Self> {
        Self::new_with_intervals(
            bridge,
            snapshotter,
            snapshot_slots,
            dirty,
            poll_interval,
            HTTP_PANE_ATTACH_LIVENESS_INTERVAL,
            HTTP_PANE_ATTACH_IDLE_MAX_POLL_INTERVAL,
        )
    }

    pub(super) fn new_with_intervals(
        bridge: BridgeSender,
        snapshotter: Arc<PaneSnapshotter>,
        snapshot_slots: Arc<Semaphore>,
        dirty: Arc<PaneDirtyRegistry>,
        poll_interval: Duration,
        liveness_interval: Duration,
        idle_poll_max_interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            sources: StdMutex::new(HashMap::new()),
            bridge,
            snapshotter,
            snapshot_slots,
            dirty,
            poll_interval,
            liveness_interval,
            idle_poll_max_interval,
        })
    }

    pub(super) async fn subscribe(
        self: &Arc<Self>,
        target: PaneAttachTarget,
    ) -> Result<PaneAttachSubscription, String> {
        let key = PaneKey::from(&target);
        if let Some(rx) = self.subscribe_existing(key) {
            return Ok(PaneAttachSubscription {
                key,
                hub: Arc::clone(self),
                rx,
            });
        }

        let dirty = self.dirty.signal_for(key);
        let _pending_registration = PendingDirtyRegistration {
            registry: &self.dirty,
            key,
            signal: &dirty,
        };
        let dirty_generation = dirty.current_generation();
        // Seed and subsequent captures use one representation. Switching ANSI
        // seed to plain text would produce an idle-only replacement/flicker.
        let snapshot = self.capture_snapshot(target.clone(), true).await?;
        let baseline = PaneAttachBaseline::from_replace_snapshot(&target, &snapshot);
        let (updates, rx) = watch::channel(PaneAttachUpdate::Replace {
            target: target.clone(),
            snapshot,
        });
        let task = self.spawn_source_task(
            key,
            target,
            baseline,
            dirty_generation,
            updates.clone(),
            Arc::clone(&dirty),
        );

        let mut sources = self
            .sources
            .lock()
            .expect("pane attach sources lock should hold");
        if let Some(source) = sources.get_mut(&key) {
            task.abort();
            source.subscribers += 1;
            return Ok(PaneAttachSubscription {
                key,
                hub: Arc::clone(self),
                rx: source.updates.subscribe(),
            });
        }

        sources.insert(
            key,
            PaneAttachSource {
                subscribers: 1,
                updates,
                task,
            },
        );

        Ok(PaneAttachSubscription {
            key,
            hub: Arc::clone(self),
            rx,
        })
    }

    fn subscribe_existing(&self, key: PaneKey) -> Option<watch::Receiver<PaneAttachUpdate>> {
        let mut sources = self
            .sources
            .lock()
            .expect("pane attach sources lock should hold");
        let source = sources.get_mut(&key)?;
        source.subscribers += 1;
        Some(source.updates.subscribe())
    }

    fn release(&self, key: PaneKey) {
        let mut sources = self
            .sources
            .lock()
            .expect("pane attach sources lock should hold");
        let Some(source) = sources.get_mut(&key) else {
            return;
        };
        source.subscribers = source.subscribers.saturating_sub(1);
        if source.subscribers == 0 {
            if let Some(source) = sources.remove(&key) {
                source.task.abort();
            }
        }
    }

    fn spawn_source_task(
        self: &Arc<Self>,
        key: PaneKey,
        target: PaneAttachTarget,
        baseline: PaneAttachBaseline,
        dirty_generation: u64,
        updates: watch::Sender<PaneAttachUpdate>,
        dirty: Arc<PaneDirtySignal>,
    ) -> AbortHandle {
        let hub = Arc::clone(self);
        let registration = SourceDirtyRegistration {
            registry: Arc::clone(&self.dirty),
            key,
            signal: dirty,
        };
        let task = tokio::spawn(async move {
            // Keep the guard across both source kinds, including tmux sources
            // whose initial capture completed after the tab was pruned.
            let registration = registration;
            match target.kind {
                PaneAttachKind::Vte => {
                    hub.run_vte_source(
                        key,
                        baseline,
                        dirty_generation,
                        updates,
                        &registration.signal,
                    )
                    .await
                }
                PaneAttachKind::Tmux { .. } => hub.run_polled_source(key, baseline, updates).await,
            }
        });
        let abort = task.abort_handle();
        drop(task);
        abort
    }

    async fn run_vte_source(
        self: Arc<Self>,
        key: PaneKey,
        mut baseline: PaneAttachBaseline,
        mut last_dirty_generation: u64,
        updates: watch::Sender<PaneAttachUpdate>,
        dirty: &PaneDirtySignal,
    ) {
        let mut consecutive_not_found_polls = 0usize;

        loop {
            tokio::select! {
                generation = dirty.await_dirty(last_dirty_generation) => {
                    last_dirty_generation = generation;
                    let target = match self.resolve_or_send_terminal_update(key, &updates, &mut consecutive_not_found_polls).await {
                        PaneAttachResolveOutcome::Target(target) => target,
                        PaneAttachResolveOutcome::Retry => continue,
                        PaneAttachResolveOutcome::Closed => break,
                    };
                    match self.capture_snapshot(target.clone(), true).await {
                        Ok(snapshot) => {
                            if baseline.has_changed(&target, &snapshot) {
                                if updates.send(PaneAttachUpdate::Replace {
                                    target: target.clone(),
                                    snapshot: snapshot.clone(),
                                }).is_err() {
                                    break;
                                }
                                baseline.update(&target, &snapshot);
                            }
                        }
                        Err(error) => {
                            trace_attach_capture_error(&target, &error);
                            let _ = updates.send(PaneAttachUpdate::Error(
                                "tmux snapshot failed; pane may have exited".to_string(),
                            ));
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(self.liveness_interval) => {
                    match self.resolve_or_send_terminal_update(key, &updates, &mut consecutive_not_found_polls).await {
                        PaneAttachResolveOutcome::Target(_) | PaneAttachResolveOutcome::Retry => {}
                        PaneAttachResolveOutcome::Closed => break,
                    }
                }
            }
        }
    }

    async fn run_polled_source(
        self: Arc<Self>,
        key: PaneKey,
        mut baseline: PaneAttachBaseline,
        updates: watch::Sender<PaneAttachUpdate>,
    ) {
        let mut interval = self.poll_interval;
        let mut consecutive_not_found_polls = 0usize;

        loop {
            tokio::time::sleep(interval).await;
            let target = match self
                .resolve_or_send_terminal_update(key, &updates, &mut consecutive_not_found_polls)
                .await
            {
                PaneAttachResolveOutcome::Target(target) => target,
                PaneAttachResolveOutcome::Retry => {
                    interval = self.next_idle_interval(interval);
                    continue;
                }
                PaneAttachResolveOutcome::Closed => break,
            };

            match self.capture_snapshot(target.clone(), true).await {
                Ok(snapshot) => {
                    if baseline.has_changed(&target, &snapshot) {
                        if updates
                            .send(PaneAttachUpdate::Replace {
                                target: target.clone(),
                                snapshot: snapshot.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        baseline.update(&target, &snapshot);
                        interval = self.poll_interval;
                    } else {
                        interval = self.next_idle_interval(interval);
                    }
                }
                Err(error) => {
                    trace_attach_capture_error(&target, &error);
                    let _ = updates.send(PaneAttachUpdate::Error(
                        "tmux snapshot failed; pane may have exited".to_string(),
                    ));
                    break;
                }
            }
        }
    }

    async fn resolve_or_send_terminal_update(
        &self,
        key: PaneKey,
        updates: &watch::Sender<PaneAttachUpdate>,
        consecutive_not_found_polls: &mut usize,
    ) -> PaneAttachResolveOutcome {
        match self.resolve_key(key).await {
            Ok(target) => {
                *consecutive_not_found_polls = 0;
                PaneAttachResolveOutcome::Target(target)
            }
            Err(error) => {
                if error.is_retryable() {
                    *consecutive_not_found_polls += 1;
                    if *consecutive_not_found_polls <= HTTP_PANE_ATTACH_NOT_FOUND_GRACE_POLLS {
                        return PaneAttachResolveOutcome::Retry;
                    }
                    let _ = updates.send(PaneAttachUpdate::Gone);
                } else {
                    let _ = updates.send(PaneAttachUpdate::Error(error.as_str().to_string()));
                }
                PaneAttachResolveOutcome::Closed
            }
        }
    }

    async fn resolve_key(&self, key: PaneKey) -> Result<PaneAttachTarget, PaneAttachResolveError> {
        resolve_current_pane_attach_target_from_bridge(&self.bridge, Some(key.tab_id), key.pane_id)
            .await
    }

    async fn capture_snapshot(
        &self,
        target: PaneAttachTarget,
        preserve_ansi: bool,
    ) -> Result<PaneSnapshot, String> {
        capture_pane_snapshot(
            Arc::clone(&self.snapshotter),
            Arc::clone(&self.snapshot_slots),
            target,
            preserve_ansi,
        )
        .await
    }

    pub(super) fn next_idle_interval(&self, interval: Duration) -> Duration {
        interval.saturating_mul(2).min(self.idle_poll_max_interval)
    }

    #[cfg(test)]
    pub(super) fn source_count(&self) -> usize {
        self.sources
            .lock()
            .expect("pane attach sources lock should hold")
            .len()
    }

    #[cfg(test)]
    pub(super) fn subscriber_count(&self, key: PaneKey) -> Option<usize> {
        self.sources
            .lock()
            .expect("pane attach sources lock should hold")
            .get(&key)
            .map(|source| source.subscribers)
    }
}

enum PaneAttachResolveOutcome {
    Target(PaneAttachTarget),
    Retry,
    Closed,
}

pub(super) struct PaneAttachSubscription {
    key: PaneKey,
    hub: Arc<PaneAttachHub>,
    pub(super) rx: watch::Receiver<PaneAttachUpdate>,
}

impl PaneAttachSubscription {
    // Mark exactly the snapshot returned to the route as seen. Updates arriving
    // while that frame is sent remain pending on the watch receiver.
    fn initial_snapshot(&mut self) -> Result<(PaneAttachTarget, PaneSnapshot), String> {
        match self.rx.borrow_and_update().clone() {
            PaneAttachUpdate::Replace { target, snapshot } => Ok((target, snapshot)),
            PaneAttachUpdate::Error(message) => Err(message),
            PaneAttachUpdate::Gone => Err(format!("pane {} was not found", self.key.pane_id)),
        }
    }
}

impl Drop for PaneAttachSubscription {
    fn drop(&mut self) {
        self.hub.release(self.key);
    }
}

async fn handle_pane_attach_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: HttpState,
    tab_id: Option<u32>,
    pane_id: u32,
    _attach_permit: OwnedSemaphorePermit,
) {
    use axum::extract::ws::Message;

    let target = match resolve_current_pane_attach_target(&state, tab_id, pane_id).await {
        Ok(target) => target,
        Err(error) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, error.as_str()).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let mut subscription = match state.pane_attach_hub.subscribe(target.clone()).await {
        Ok(subscription) => subscription,
        Err(error) => {
            trace_attach_capture_error(&target, &error);
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(
                        target.pane_id,
                        "tmux snapshot failed; pane may have exited",
                    )
                    .to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let (seed_target, snapshot) = match subscription.initial_snapshot() {
        Ok(seed) => seed,
        Err(message) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &message).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let snapshot_frame = snapshot_frame_json(&seed_target, &snapshot);
    trace_attach_frame("snapshot", &seed_target, &snapshot_frame);
    if socket
        .send(Message::text(snapshot_frame.to_string()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            update = subscription.rx.changed() => {
                if update.is_err() {
                    break;
                }
                if send_terminal_update_if_closed(&mut socket, pane_id, &mut subscription).await {
                    break;
                }
            }
        }
    }
}

async fn handle_pane_control_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: HttpState,
    tab_id: u32,
    pane_id: u32,
    _attach_permit: OwnedSemaphorePermit,
) {
    use axum::extract::ws::Message;

    let target = match resolve_current_pane_attach_target(&state, Some(tab_id), pane_id).await {
        Ok(target) => target,
        Err(error) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, error.as_str()).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    emit_pane_control_lifecycle_event(&state, "opened", &target, None);

    let mut subscription = match state.pane_attach_hub.subscribe(target.clone()).await {
        Ok(subscription) => subscription,
        Err(error) => {
            trace_attach_capture_error(&target, &error);
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(
                        target.pane_id,
                        "tmux snapshot failed; pane may have exited",
                    )
                    .to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            emit_pane_control_lifecycle_event(&state, "closed", &target, Some("subscribe_failed"));
            return;
        }
    };

    let (seed_target, snapshot) = match subscription.initial_snapshot() {
        Ok(seed) => seed,
        Err(message) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &message).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            emit_pane_control_lifecycle_event(
                &state,
                "closed",
                &target,
                Some("initial_update_closed"),
            );
            return;
        }
    };
    let snapshot_frame = snapshot_frame_json(&seed_target, &snapshot);
    trace_attach_frame("control-snapshot", &seed_target, &snapshot_frame);
    if socket
        .send(Message::text(snapshot_frame.to_string()))
        .await
        .is_err()
    {
        emit_pane_control_lifecycle_event(&state, "closed", &target, Some("send_failed"));
        return;
    }

    let mut last_control_output = snapshot.output.clone();
    let mut last_control_size = (snapshot.width, snapshot.height);

    let mut close_reason = "client_closed";
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(payload))) => {
                        match handle_pane_control_client_text(&mut socket, &state, tab_id, pane_id, payload.as_str()).await {
                            Ok(()) => {}
                            Err(reason) => {
                                close_reason = reason;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = socket
                            .send(Message::text(
                                pane_attach_error_frame(
                                    pane_id,
                                    "binary control frames are not supported",
                                )
                                .to_string(),
                            ))
                            .await;
                        close_reason = "unsupported_binary_frame";
                        break;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Err(_)) => {
                        close_reason = "socket_error";
                        break;
                    }
                }
            }
            update = subscription.rx.changed() => {
                if update.is_err() {
                    close_reason = "subscription_closed";
                    break;
                }
                if send_control_terminal_update_if_closed(
                    &mut socket,
                    pane_id,
                    &mut subscription,
                    &mut last_control_output,
                    &mut last_control_size,
                ).await {
                    close_reason = "pane_update_closed";
                    break;
                }
            }
        }
    }

    let _ = socket.send(Message::Close(None)).await;
    emit_pane_control_lifecycle_event(&state, "closed", &target, Some(close_reason));
}

async fn handle_pane_control_client_text(
    socket: &mut axum::extract::ws::WebSocket,
    state: &HttpState,
    tab_id: u32,
    pane_id: u32,
    payload: &str,
) -> Result<(), &'static str> {
    use axum::extract::ws::Message;

    let frame: PaneControlClientFrame = match serde_json::from_str(payload) {
        Ok(frame) => frame,
        Err(_) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, "invalid pane control frame").to_string(),
                ))
                .await;
            return Err("invalid_frame");
        }
    };

    match frame {
        PaneControlClientFrame::Input { payload, encoding } => {
            if !matches!(encoding.as_deref().unwrap_or("utf8"), "utf8") {
                let _ = socket
                    .send(Message::text(
                        pane_attach_error_frame(pane_id, "unsupported input frame encoding")
                            .to_string(),
                    ))
                    .await;
                return Err("unsupported_input_encoding");
            }
            if payload.is_empty() {
                return Ok(());
            }
            match send_control_action_value(
                state,
                HttpControlAction::SendKeys {
                    tab: Some(tab_id.to_string()),
                    pane: pane_id,
                    keys: payload,
                },
            )
            .await
            {
                Ok(_) => Ok(()),
                Err(error) => {
                    let _ = socket
                        .send(Message::text(
                            pane_attach_error_frame(pane_id, &control_route_error_message(error))
                                .to_string(),
                        ))
                        .await;
                    Err("input_dispatch_failed")
                }
            }
        }
        PaneControlClientFrame::Resize { cols, rows } => {
            if let Err(message) = validate_pane_control_resize(cols, rows) {
                let _ = socket
                    .send(Message::text(
                        pane_control_resize_failure_frame(pane_id, cols, rows, message).to_string(),
                    ))
                    .await;
                return Ok(());
            }
            match send_control_action_value(
                state,
                HttpControlAction::ResizePane {
                    tab: Some(tab_id.to_string()),
                    pane: pane_id,
                    cols,
                    rows,
                },
            )
            .await
            {
                Ok(response) => {
                    let _ = socket
                        .send(Message::text(
                            pane_control_resize_frame(pane_id, cols, rows, &response).to_string(),
                        ))
                        .await;
                    Ok(())
                }
                Err(error) => {
                    let _ = socket
                        .send(Message::text(
                            pane_control_resize_failure_frame(
                                pane_id,
                                cols,
                                rows,
                                &control_route_error_message(error),
                            )
                            .to_string(),
                        ))
                        .await;
                    Ok(())
                }
            }
        }
    }
}

fn control_route_error_message(error: ControlRouteError) -> String {
    match error {
        ControlRouteError::Status(status) => {
            format!("control action failed with status {}", status.as_u16())
        }
        ControlRouteError::Runtime(message) => message,
    }
}

fn validate_pane_control_resize(cols: u32, rows: u32) -> Result<(), &'static str> {
    if cols == 0 || rows == 0 {
        return Err("resize dimensions must be positive");
    }
    if cols > PANE_CONTROL_MAX_COLS || rows > PANE_CONTROL_MAX_ROWS {
        return Err("resize dimensions exceed supported bounds");
    }
    Ok(())
}

fn pane_control_resize_frame(pane_id: u32, cols: u32, rows: u32, response: &Value) -> Value {
    let data = response.get("data").unwrap_or(response);
    let supported = data
        .get("supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("backend resize is not supported for this pane");

    json!({
        "type": "resize",
        "pane_id": pane_id,
        "cols": cols,
        "rows": rows,
        "supported": supported,
        "message": message,
    })
}

fn pane_control_resize_failure_frame(pane_id: u32, cols: u32, rows: u32, message: &str) -> Value {
    json!({
        "type": "resize",
        "pane_id": pane_id,
        "cols": cols,
        "rows": rows,
        "supported": false,
        "message": message,
    })
}

fn emit_pane_control_lifecycle_event(
    state: &HttpState,
    lifecycle: &'static str,
    target: &PaneAttachTarget,
    reason: Option<&'static str>,
) {
    let mut payload = json!({
        "lifecycle": lifecycle,
        "tab_id": target.tab_id,
        "pane_id": target.pane_id,
        "attach_kind": pane_attach_kind_label(&target.kind),
    });
    if let Some(reason) = reason {
        if let Some(map) = payload.as_object_mut() {
            map.insert("reason".to_string(), json!(reason));
        }
    }

    let _ = state.bridge.try_send(HttpBridgeRequest::EmitEvent {
        event_type: "http_pane_control_lifecycle".to_string(),
        payload,
    });
}

async fn send_control_terminal_update_if_closed(
    socket: &mut axum::extract::ws::WebSocket,
    pane_id: u32,
    subscription: &mut PaneAttachSubscription,
    last_output: &mut String,
    last_size: &mut (u32, u32),
) -> bool {
    use axum::extract::ws::Message;

    let update = subscription.rx.borrow_and_update().clone();
    match update {
        PaneAttachUpdate::Replace { target, snapshot } => {
            let Some(update_frame) =
                control_terminal_update_frame_json(&target, &snapshot, last_output, *last_size)
            else {
                return false;
            };
            trace_attach_frame(
                update_frame
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("control-update"),
                &target,
                &update_frame,
            );
            let send_failed = socket
                .send(Message::text(update_frame.to_string()))
                .await
                .is_err();
            if !send_failed {
                *last_output = snapshot.output;
                *last_size = (snapshot.width, snapshot.height);
            }
            send_failed
        }
        PaneAttachUpdate::Error(message) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &message).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            true
        }
        PaneAttachUpdate::Gone => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &format!("pane {pane_id} was not found"))
                        .to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            true
        }
    }
}

async fn send_terminal_update_if_closed(
    socket: &mut axum::extract::ws::WebSocket,
    pane_id: u32,
    subscription: &mut PaneAttachSubscription,
) -> bool {
    use axum::extract::ws::Message;

    let update = subscription.rx.borrow_and_update().clone();
    match update {
        PaneAttachUpdate::Replace { target, snapshot } => {
            let replace_frame = replace_frame_json(&target, &snapshot);
            trace_attach_frame("replace", &target, &replace_frame);
            socket
                .send(Message::text(replace_frame.to_string()))
                .await
                .is_err()
        }
        PaneAttachUpdate::Error(message) => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &message).to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            true
        }
        PaneAttachUpdate::Gone => {
            let _ = socket
                .send(Message::text(
                    pane_attach_error_frame(pane_id, &format!("pane {pane_id} was not found"))
                        .to_string(),
                ))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            true
        }
    }
}

async fn capture_pane_snapshot(
    snapshotter: Arc<PaneSnapshotter>,
    snapshot_slots: Arc<Semaphore>,
    target: PaneAttachTarget,
    preserve_ansi: bool,
) -> Result<PaneSnapshot, String> {
    let _snapshot_permit = tokio::time::timeout(
        HTTP_PANE_ATTACH_SNAPSHOT_SLOT_TIMEOUT,
        snapshot_slots.acquire_owned(),
    )
    .await
    .map_err(|_| "pane attach snapshot queue timed out".to_string())?
    .map_err(|_| "pane attach snapshot limiter closed".to_string())?;
    snapshotter(target, preserve_ansi).await
}

async fn capture_tmux_snapshot(
    target: PaneAttachTarget,
    preserve_ansi: bool,
) -> Result<PaneSnapshot, String> {
    let PaneAttachKind::Tmux {
        session_name,
        target: tmux_target,
    } = target.kind
    else {
        return Err("capture_tmux_snapshot called with non-tmux target".to_string());
    };
    tokio::task::spawn_blocking(move || {
        crate::tmux::capture_pane_snapshot(&tmux_target, &session_name, preserve_ansi)
    })
    .await
    .map_err(|error| format!("pane attach worker failed: {error}"))?
    .map(PaneSnapshot::from)
}

async fn capture_vte_snapshot(
    bridge: BridgeSender,
    target: PaneAttachTarget,
) -> Result<PaneSnapshot, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    bridge
        .send(HttpBridgeRequest::CaptureVtePaneSnapshot {
            tab_id: target.tab_id,
            pane_id: target.pane_id,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "pane attach bridge closed".to_string())?;
    reply_rx
        .await
        .map_err(|_| "pane attach bridge dropped reply".to_string())?
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub output: String,
    pub width: u32,
    pub height: u32,
}

impl From<crate::tmux::TmuxPaneSnapshot> for PaneSnapshot {
    fn from(snapshot: crate::tmux::TmuxPaneSnapshot) -> Self {
        Self {
            output: snapshot.output,
            width: snapshot.width,
            height: snapshot.height,
        }
    }
}

fn pane_attach_trace_enabled() -> bool {
    std::env::var("TAAROF_PANE_ATTACH_TRACE")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn pane_attach_kind_label(kind: &PaneAttachKind) -> &'static str {
    match kind {
        PaneAttachKind::Tmux { .. } => "tmux",
        PaneAttachKind::Vte => "vte",
    }
}

fn trace_preview(text: &str) -> String {
    text.chars()
        .take(160)
        .collect::<String>()
        .replace('\n', "\\n")
}

fn trace_vte_capture(
    tab_id: u32,
    pane_id: u32,
    start_row: libc::c_long,
    cursor_row: libc::c_long,
    snapshot: &PaneSnapshot,
) {
    if !pane_attach_trace_enabled() {
        return;
    }

    eprintln!(
        "taarof: pane-attach-trace vte-capture tab_id={tab_id} pane_id={pane_id} start_row={start_row} cursor_row={cursor_row} cols={} rows={} bytes={} preview={:?}",
        snapshot.width,
        snapshot.height,
        snapshot.output.len(),
        trace_preview(&snapshot.output),
    );
}

fn trace_attach_frame(frame_type: &str, target: &PaneAttachTarget, frame: &Value) {
    if !pane_attach_trace_enabled() {
        return;
    }

    let payload = frame.get("payload").and_then(Value::as_str).unwrap_or("");
    let encoding = frame
        .get("encoding")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    eprintln!(
        "taarof: pane-attach-trace ws-frame type={frame_type} tab_id={} pane_id={} kind={} encoding={encoding} cols={} rows={} payload_chars={} payload_preview={:?}",
        target.tab_id,
        target.pane_id,
        pane_attach_kind_label(&target.kind),
        frame.get("cols").and_then(Value::as_u64).unwrap_or(0),
        frame.get("rows").and_then(Value::as_u64).unwrap_or(0),
        payload.len(),
        trace_preview(payload),
    );
}

fn trace_attach_capture_error(target: &PaneAttachTarget, error: &str) {
    eprintln!(
        "taarof: tmux snapshot failed for pane {pane_id}: {error}",
        pane_id = target.pane_id,
        error = error,
    );
}

pub(super) fn snapshot_frame_json(target: &PaneAttachTarget, snapshot: &PaneSnapshot) -> Value {
    json!({
        "type": "snapshot",
        "pane_id": target.pane_id,
        "cols": snapshot.width,
        "rows": snapshot.height,
        "encoding": "base64",
        "payload": base64::engine::general_purpose::STANDARD.encode(snapshot.output.as_bytes()),
    })
}

pub(super) fn replace_frame_json(target: &PaneAttachTarget, snapshot: &PaneSnapshot) -> Value {
    json!({
        "type": "replace",
        "pane_id": target.pane_id,
        "cols": snapshot.width,
        "rows": snapshot.height,
        "encoding": "utf8",
        "payload": snapshot.output,
    })
}

pub(super) fn control_terminal_update_frame_json(
    target: &PaneAttachTarget,
    snapshot: &PaneSnapshot,
    last_output: &str,
    last_size: (u32, u32),
) -> Option<Value> {
    let current_size = (snapshot.width, snapshot.height);
    if current_size == last_size && snapshot.output.starts_with(last_output) {
        let delta = &snapshot.output[last_output.len()..];
        if delta.is_empty() {
            return None;
        }
        return Some(pane_control_delta_frame_json(target, snapshot, delta));
    }

    Some(replace_frame_json(target, snapshot))
}

fn pane_control_delta_frame_json(
    target: &PaneAttachTarget,
    snapshot: &PaneSnapshot,
    payload: &str,
) -> Value {
    json!({
        "type": "delta",
        "pane_id": target.pane_id,
        "cols": snapshot.width,
        "rows": snapshot.height,
        "encoding": "utf8",
        "payload": payload,
    })
}

fn pane_attach_error_frame(pane_id: u32, error: &str) -> Value {
    json!({
        "type": "error",
        "pane_id": pane_id,
        "error": error,
    })
}

#[cfg(test)]
mod tab_cleanup_tests {
    use super::*;

    #[tokio::test]
    async fn tab_cleanup_late_source_completion_and_abort_release_registration() {
        for (abort_before_poll, kind) in [
            (false, PaneAttachKind::Vte),
            (true, PaneAttachKind::Vte),
            (
                false,
                PaneAttachKind::Tmux {
                    session_name: "inert".into(),
                    target: crate::tmux::TmuxTarget::Local,
                },
            ),
            (
                true,
                PaneAttachKind::Tmux {
                    session_name: "inert".into(),
                    target: crate::tmux::TmuxTarget::Local,
                },
            ),
        ] {
            let (bridge, mut requests) = mpsc::channel(8);
            let registry = Arc::new(PaneDirtyRegistry::default());
            let hub = PaneAttachHub::new_with_intervals(
                bridge,
                Arc::new(|_, _| {
                    Box::pin(async {
                        Ok(PaneSnapshot {
                            output: "inert".into(),
                            width: 80,
                            height: 24,
                        })
                    })
                }),
                Arc::new(Semaphore::new(1)),
                Arc::clone(&registry),
                Duration::from_millis(1),
                Duration::from_millis(1),
                Duration::from_millis(1),
            );
            // GTK already resolved this target; close/prune wins before subscribe.
            registry.retain_tabs(&HashSet::new());
            let target = PaneAttachTarget {
                tab_id: 4,
                pane_id: 0,
                kind,
            };
            let responder = tokio::spawn(async move {
                while let Some(HttpBridgeRequest::ResolvePaneAttach { reply, .. }) =
                    requests.recv().await
                {
                    let _ = reply.send(PaneAttachLookup::NotFound);
                }
            });
            let subscription = hub.subscribe(target).await.unwrap();
            assert_eq!(registry.signals.lock().unwrap().len(), 1);
            if abort_before_poll {
                // subscribe has no yielding operation in this inert capture;
                // source is cancelled before Tokio first polls its future.
                drop(subscription);
            } else {
                let mut subscription = subscription;
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        subscription.rx.changed().await.unwrap();
                        if matches!(*subscription.rx.borrow(), PaneAttachUpdate::Gone) {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
                drop(subscription);
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                while !registry.signals.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("ended source must release late registration without another prune");
            responder.abort();
        }
    }

    #[test]
    fn tab_cleanup_dirty_callbacks_and_failed_capture_do_not_recreate_entries() {
        let registry = PaneDirtyRegistry::default();
        let key = PaneKey {
            tab_id: 4,
            pane_id: 0,
        };
        let dirty = registry.signal_for(key);
        assert!(registry.mark_dirty(4, 0));
        registry.retain_tabs(&HashSet::new());
        assert!(!registry.mark_dirty(4, 0));
        assert!(registry.signals.lock().unwrap().is_empty());
        // An already-spawned source owns this Arc directly and can observe its
        // generation without looking the deleted key up in the registry again.
        assert_eq!(dirty.current_generation(), 1);

        let failed_capture = registry.signal_for(key);
        {
            let _cancelled = PendingDirtyRegistration {
                registry: &registry,
                key,
                signal: &failed_capture,
            };
        }
        assert!(registry.signals.lock().unwrap().is_empty());
        let active = registry.signal_for(key);
        let second_capture = registry.signal_for(key);
        registry.release_unused(key, &second_capture);
        assert_eq!(registry.signals.lock().unwrap().len(), 1);
        assert!(Arc::ptr_eq(&active, &second_capture));
    }
}
