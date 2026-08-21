use axum::{
    extract::{Path as AxumPath, Query, State, WebSocketUpgrade},
    http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

mod auth;
mod pane_attach;
mod pty;
mod web_assets;

pub use self::auth::{cleanup_token_file, token_path};
pub(crate) use self::pane_attach::{
    capture_vte_pane_snapshot_sync, resolve_pane_attach_target, resolve_pane_attach_target_in_tab,
};
pub use self::pane_attach::{
    PaneAttachKind, PaneAttachLookup, PaneAttachTarget, PaneDirtyRegistry, PaneKey, PaneSnapshot,
};
use self::pty::tab_pane_pty_ws;
pub(crate) use self::pty::{dispatch_pty_input, dispatch_pty_resize, resolve_pty_adapter_target};
pub use self::pty::{PtyAdapterHandle, PtyAdapterResolution, PtyDispatchGuard};

use self::auth::{
    check_auth, check_ws_auth, generate_token, http_control_enabled_for_bind, resolve_bind_addr,
    warn_on_non_loopback_bind, write_token_file,
};
use self::pane_attach::{
    default_pane_snapshotter, pane_attach_ws, tab_pane_attach_control_ws, tab_pane_attach_ws,
    PaneAttachHub, WebSocketAuthQuery,
};
use self::web_assets::{serve_web_request, web_asset_candidate_paths};
pub use crate::api::StateProjection;

// ── Bridge types ──
// HTTP handlers send requests across a bounded runtime bridge.
// The GTK thread owns the Receiver and processes requests on the GLib main context.

#[cfg(test)]
use self::pane_attach::{
    control_terminal_update_frame_json, replace_frame_json, snapshot_frame_json,
    PaneAttachBaseline, PaneAttachUpdate,
};
#[cfg(test)]
use self::web_assets::{
    installed_web_dist_dir_from_exe_path, missing_web_dist_diagnostic, resolve_web_assets,
    sanitize_web_path, source_checkout_root_from_exe_path, web_asset_candidate_paths_for,
    MISSING_WEB_DIST_DIAGNOSTIC_PREFIX, MISSING_WEB_DIST_RESPONSE_BODY,
};

const HTTP_BRIDGE_CAPACITY: usize = 256;
const HTTP_BRIDGE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CONTROL_CALLBACK_MARGIN: Duration = Duration::from_secs(2);
const HTTP_TOKEN_BYTES: usize = 32;
const HTTP_REMOTE_BIND_OPT_IN: &str = "[http].unsafe_allow_non_loopback = true";
const HTTP_WEB_DIST_ENV: &str = "TAAROF_WEB_DIST_DIR";
const HTTP_PANE_ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HTTP_PANE_ATTACH_LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
const HTTP_PANE_ATTACH_IDLE_MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
const HTTP_PANE_ATTACH_NOT_FOUND_GRACE_POLLS: usize = 3;
const HTTP_PANE_ATTACH_MAX_CONNECTIONS: usize = 16;
const HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS: usize = 4;
const HTTP_PANE_ATTACH_SNAPSHOT_SLOT_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_FILE_PREVIEW_MAX_BYTES: u64 = 256 * 1024;

pub enum HttpBridgeRequest {
    QueryState {
        reply: oneshot::Sender<Value>,
    },
    QueryStateProjection {
        projection: StateProjection,
        reply: oneshot::Sender<Value>,
    },
    QueryHealth {
        reply: oneshot::Sender<Value>,
    },
    QueryEvents {
        since_seq: Option<u64>,
        limit: Option<usize>,
        reply: oneshot::Sender<Value>,
    },
    QueryAgentBindings {
        reply: oneshot::Sender<Vec<crate::agent_sessions::LiveAgentBinding>>,
    },
    ResolvePaneAttach {
        tab_id: Option<u32>,
        pane_id: u32,
        reply: oneshot::Sender<PaneAttachLookup>,
    },
    CaptureVtePaneSnapshot {
        tab_id: u32,
        pane_id: u32,
        reply: oneshot::Sender<Result<PaneSnapshot, String>>,
    },
    ResolvePtyAdapter {
        tab_id: u32,
        pane_id: u32,
        reply: oneshot::Sender<PtyAdapterResolution>,
    },
    DispatchPtyInput {
        tab_id: u32,
        pane_id: u32,
        guard: PtyDispatchGuard,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    DispatchPtyResize {
        tab_id: u32,
        pane_id: u32,
        guard: PtyDispatchGuard,
        cols: u16,
        rows: u16,
        reply: oneshot::Sender<Result<(), String>>,
    },
    EmitEvent {
        event_type: String,
        payload: Value,
    },
    ControlAction {
        action: HttpControlAction,
        guard: HttpControlRequestGuard,
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

pub type BridgeSender = mpsc::Sender<HttpBridgeRequest>;
pub type BridgeReceiver = mpsc::Receiver<HttpBridgeRequest>;
type PaneSnapshotFuture = Pin<Box<dyn Future<Output = Result<PaneSnapshot, String>> + Send>>;
type PaneSnapshotter = dyn Fn(PaneAttachTarget, bool) -> PaneSnapshotFuture + Send + Sync;

#[derive(Clone)]
struct HttpState {
    bridge: BridgeSender,
    auth_token: Arc<String>,
    control_enabled: bool,
    bind_is_loopback: bool,
    runtime_id: Arc<String>,
    agent_session_catalog: Arc<crate::agent_sessions::AgentSessionCatalog>,
    event_broadcast: broadcast::Sender<Value>,
    web_asset_candidates: Arc<Vec<PathBuf>>,
    pane_snapshotter: Arc<PaneSnapshotter>,
    pane_attach_slots: Arc<Semaphore>,
    pane_snapshot_slots: Arc<Semaphore>,
    pane_attach_hub: Arc<PaneAttachHub>,
    history_reader: crate::history::HistoryReader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpControlAction {
    SendKeys {
        tab: Option<String>,
        pane: u32,
        keys: String,
    },
    RunInPane {
        tab: Option<String>,
        pane: u32,
        command: String,
    },
    ResizePane {
        tab: Option<String>,
        pane: u32,
        cols: u32,
        rows: u32,
    },
    SwitchTab {
        tab: String,
    },
    CreateTab {
        name: Option<String>,
        working_dir: Option<String>,
        command: Option<String>,
    },
    SplitPane {
        tab: Option<String>,
        direction: Option<String>,
        command: Option<String>,
        working_dir: Option<String>,
    },
}

impl HttpControlAction {
    fn bridge_response_timeout(&self) -> Duration {
        match self {
            Self::SendKeys { .. } | Self::ResizePane { .. } => {
                crate::tmux::TMUX_CONTROL_DEADLINE + HTTP_CONTROL_CALLBACK_MARGIN
            }
            Self::RunInPane { .. }
            | Self::SwitchTab { .. }
            | Self::CreateTab { .. }
            | Self::SplitPane { .. } => HTTP_BRIDGE_RESPONSE_TIMEOUT,
        }
    }
}

const CONTROL_REQUEST_PENDING: u8 = 0;
const CONTROL_REQUEST_APPLYING: u8 = 1;
const CONTROL_REQUEST_CANCELLED: u8 = 2;
const CONTROL_REQUEST_FINISHED: u8 = 3;

/// Coordinates the synchronous HTTP timeout with the main-context mutation.
/// A timeout may claim a still-pending request and prohibit a later apply. If
/// the main context already claimed the apply, HTTP waits for its exact result
/// instead of returning an ambiguous 504 that callers might retry.
#[derive(Clone, Debug)]
pub struct HttpControlRequestGuard {
    phase: Arc<AtomicU8>,
}

impl HttpControlRequestGuard {
    fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(CONTROL_REQUEST_PENDING)),
        }
    }

    pub(crate) fn try_begin_apply(&self) -> bool {
        self.phase
            .compare_exchange(
                CONTROL_REQUEST_PENDING,
                CONTROL_REQUEST_APPLYING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel_pending(&self) -> bool {
        self.phase
            .compare_exchange(
                CONTROL_REQUEST_PENDING,
                CONTROL_REQUEST_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn finish(&self) {
        let _ = self.phase.compare_exchange(
            CONTROL_REQUEST_APPLYING,
            CONTROL_REQUEST_FINISHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

// ── Handlers ──

async fn health(State(state): State<HttpState>) -> impl IntoResponse {
    match query_health_snapshot(&state).await {
        Ok(health) => {
            let degraded = health
                .get("degraded")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let runtime_probe = health.get("runtime_probe").map(|probe| {
                json!({
                    "state": probe.get("state"),
                    "process_state": probe.pointer("/process/state"),
                    "ports_state": probe.pointer("/ports/state"),
                })
            });
            Json(json!({
                "ok": true,
                "degraded": degraded,
                "state": health.get("state"),
                "runtime_probe": runtime_probe,
            }))
            .into_response()
        }
        Err(status) => (
            status,
            Json(json!({
                "ok": false,
                "error": "could not query runtime health",
            })),
        )
            .into_response(),
    }
}

fn bridge_overload_status(error: mpsc::error::TrySendError<HttpBridgeRequest>) -> StatusCode {
    match error {
        mpsc::error::TrySendError::Full(_) => StatusCode::SERVICE_UNAVAILABLE,
        mpsc::error::TrySendError::Closed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn await_bridge_reply<T>(reply_rx: oneshot::Receiver<T>) -> Result<T, StatusCode> {
    tokio::time::timeout(HTTP_BRIDGE_RESPONSE_TIMEOUT, reply_rx)
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn await_control_bridge_reply<T>(
    mut reply_rx: oneshot::Receiver<T>,
    guard: &HttpControlRequestGuard,
    timeout: Duration,
) -> Result<T, StatusCode> {
    match tokio::time::timeout(timeout, &mut reply_rx).await {
        Ok(reply) => reply.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        Err(_) if guard.cancel_pending() => Err(StatusCode::GATEWAY_TIMEOUT),
        Err(_) => reply_rx
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn query_state_snapshot(state: &HttpState) -> Result<Value, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryState { reply: reply_tx })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

async fn query_state_projection(
    state: &HttpState,
    projection: StateProjection,
) -> Result<Value, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryStateProjection {
            projection,
            reply: reply_tx,
        })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

async fn query_health_snapshot(state: &HttpState) -> Result<Value, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryHealth { reply: reply_tx })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

async fn query_agent_bindings(
    state: &HttpState,
) -> Result<Vec<crate::agent_sessions::LiveAgentBinding>, StatusCode> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryAgentBindings { reply: reply_tx })
        .map_err(bridge_overload_status)?;
    await_bridge_reply(reply_rx).await
}

async fn get_state(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryState { reply: reply_tx })
        .map_err(bridge_overload_status)?;
    let data = await_bridge_reply(reply_rx).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

async fn get_runtime_identity(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    Ok(Json(json!({
        "ok": true,
        "data": {
            "schema": "taarof.runtime-identity.v1",
            "runtime_id": state.runtime_id.as_ref(),
            "session_name": crate::instance::session_name()
                .unwrap_or_else(|| "default".to_string()),
            // Additive (EXAMPLE-164). `schema`/`runtime_id` are unchanged so the
            // gateway's runtime-id pin keeps working; `identity` lets the same
            // route answer "which build is this" without a second call.
            "identity": crate::runtime_identity::cached(),
        }
    })))
}

#[derive(Deserialize)]
struct EventsQuery {
    since_seq: Option<u64>,
    limit: Option<usize>,
}

async fn get_events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<EventsQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::QueryEvents {
            since_seq: params.since_seq,
            limit: params.limit,
            reply: reply_tx,
        })
        .map_err(bridge_overload_status)?;
    let data = await_bridge_reply(reply_rx).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    since_id: Option<u64>,
    limit: Option<usize>,
    from_ts: Option<u64>,
    to_ts: Option<u64>,
    record_type: Option<String>,
    session: Option<String>,
    workspace: Option<String>,
    pane: Option<String>,
    task: Option<String>,
    repository: Option<String>,
    authority: Option<String>,
    verification: Option<String>,
    severity: Option<String>,
    text: Option<String>,
    #[serde(default)]
    order: crate::history::HistoryOrder,
    scan_budget: Option<usize>,
}

impl HistoryQuery {
    fn into_filters(self) -> crate::history::HistoryFilters {
        crate::history::HistoryFilters {
            from_ts: self.from_ts,
            to_ts: self.to_ts,
            record_type: self.record_type,
            session: self.session,
            workspace: self.workspace,
            pane: self.pane,
            task: self.task,
            repository: self.repository,
            authority: self.authority,
            verification: self.verification,
            severity: self.severity,
            text: self.text,
            order: self.order,
            scan_budget: self.scan_budget,
        }
    }
}

struct CancelHistoryQueryOnDrop(crate::history::HistoryQueryToken);

impl Drop for CancelHistoryQueryOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn get_history(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<HistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    check_auth(&headers, &state.auth_token)
        .map_err(|status| (status, Json(json!({"ok": false, "error": "unauthorized"}))))?;
    let reader = state.history_reader.clone();
    let since_id = params.since_id;
    let limit = params.limit;
    let filters = params.into_filters();
    // If the HTTP request future is dropped (including a disconnected client),
    // interrupt the SQLite VM still running in the blocking worker.
    let token = crate::history::HistoryQueryToken::new();
    let worker_token = token.clone();
    let _cancel_on_drop = CancelHistoryQueryOnDrop(token);
    let page = tokio::task::spawn_blocking(move || {
        reader.query_with_token(since_id, limit, filters, &worker_token)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": "history query worker failed"})),
        )
    })?
    .map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error})),
        )
    })?;
    Ok(Json(json!({"ok": true, "data": page})))
}

#[derive(Deserialize)]
struct SendKeysRequest {
    tab: Option<String>,
    pane: u32,
    keys: String,
}

#[derive(Deserialize)]
struct RunInPaneRequest {
    tab: Option<String>,
    pane: u32,
    command: String,
}

#[derive(Deserialize)]
struct SwitchTabRequest {
    tab: String,
}

#[derive(Deserialize)]
struct CreateTabRequest {
    name: Option<String>,
    working_dir: Option<String>,
    command: Option<String>,
}

#[derive(Deserialize)]
struct SplitPaneRequest {
    tab: Option<String>,
    direction: Option<String>,
    command: Option<String>,
    working_dir: Option<String>,
}

#[derive(Deserialize)]
struct FilePreviewQuery {
    tab: u32,
    pane: u32,
    path: String,
    line: Option<u32>,
    col: Option<u32>,
}

fn check_control_enabled(state: &HttpState) -> Result<(), StatusCode> {
    if state.control_enabled {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

enum ControlRouteError {
    Status(StatusCode),
    Runtime(String),
}

impl From<StatusCode> for ControlRouteError {
    fn from(status: StatusCode) -> Self {
        Self::Status(status)
    }
}

impl IntoResponse for ControlRouteError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status) => status.into_response(),
            Self::Runtime(error) => (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": error,
                })),
            )
                .into_response(),
        }
    }
}

enum FilePreviewRouteError {
    Status(StatusCode),
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Unsupported(String),
    TooLarge(String),
}

impl From<StatusCode> for FilePreviewRouteError {
    fn from(status: StatusCode) -> Self {
        Self::Status(status)
    }
}

impl IntoResponse for FilePreviewRouteError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Status(status) => return status.into_response(),
            Self::BadRequest(error) => (StatusCode::BAD_REQUEST, error),
            Self::Forbidden(error) => (StatusCode::FORBIDDEN, error),
            Self::NotFound(error) => (StatusCode::NOT_FOUND, error),
            Self::Unsupported(error) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, error),
            Self::TooLarge(error) => (StatusCode::PAYLOAD_TOO_LARGE, error),
        };
        (
            status,
            Json(json!({
                "ok": false,
                "error": error,
            })),
        )
            .into_response()
    }
}

async fn send_control_action(
    state: &HttpState,
    action: HttpControlAction,
) -> Result<Json<Value>, ControlRouteError> {
    let data = send_control_action_value(state, action).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

async fn send_control_action_value(
    state: &HttpState,
    action: HttpControlAction,
) -> Result<Value, ControlRouteError> {
    let response_timeout = action.bridge_response_timeout();
    send_control_action_value_with_timeout(state, action, response_timeout).await
}

async fn send_control_action_value_with_timeout(
    state: &HttpState,
    action: HttpControlAction,
    response_timeout: Duration,
) -> Result<Value, ControlRouteError> {
    let guard = HttpControlRequestGuard::new();
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .bridge
        .try_send(HttpBridgeRequest::ControlAction {
            action,
            guard: guard.clone(),
            reply: reply_tx,
        })
        .map_err(bridge_overload_status)?;
    let data = await_control_bridge_reply(reply_rx, &guard, response_timeout)
        .await?
        .map_err(ControlRouteError::Runtime)?;
    Ok(data)
}

async fn file_preview(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<FilePreviewQuery>,
) -> Result<Json<Value>, FilePreviewRouteError> {
    check_auth(&headers, &state.auth_token)?;
    let snapshot = query_state_snapshot(&state).await?;
    let pane = find_state_pane(&snapshot, params.tab, params.pane)
        .ok_or_else(|| FilePreviewRouteError::NotFound("pane not found".to_string()))?;
    let preview = build_file_preview_from_pane(&pane, &params).await?;
    Ok(Json(json!({ "ok": true, "data": preview })))
}

async fn file_stat(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(params): Query<FilePreviewQuery>,
) -> Result<Json<Value>, FilePreviewRouteError> {
    check_auth(&headers, &state.auth_token)?;
    let snapshot = query_state_snapshot(&state).await?;
    let pane = find_state_pane(&snapshot, params.tab, params.pane)
        .ok_or_else(|| FilePreviewRouteError::NotFound("pane not found".to_string()))?;
    let stat = build_file_stat_from_pane(&pane, &params).await?;
    Ok(Json(json!({ "ok": true, "data": stat })))
}

fn find_state_pane(snapshot: &Value, tab_id: u32, pane_id: u32) -> Option<Value> {
    let workspaces = snapshot.get("workspaces")?.as_array()?;
    for workspace in workspaces {
        for tab in workspace.get("tabs")?.as_array()? {
            if tab.get("tab_id")?.as_u64()? != u64::from(tab_id) {
                continue;
            }
            for pane in tab.get("panes")?.as_array()? {
                if pane.get("pane_id")?.as_u64()? == u64::from(pane_id) {
                    return Some(pane.clone());
                }
            }
        }
    }
    None
}

async fn build_file_preview_from_pane(
    pane: &Value,
    params: &FilePreviewQuery,
) -> Result<Value, FilePreviewRouteError> {
    let remote_shell = pane
        .get("remote_shell")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cwd_host = pane.get("cwd_host").and_then(Value::as_str);
    // A pane whose OSC7 host is this machine's own hostname is local; mirror the
    // GTK terminal, which resolves such references locally via `is_remote_host`.
    // Without this, ordinary local panes (OSC7 emits the hostname) would be
    // rejected as remote and the browser preview would never open.
    let has_remote_host = crate::terminal::is_remote_host(cwd_host);
    if remote_shell || has_remote_host {
        return Err(FilePreviewRouteError::Unsupported(
            "file preview is only available for local panes".to_string(),
        ));
    }

    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| FilePreviewRouteError::BadRequest("pane cwd is unknown".to_string()))?;
    let (base, resolved) = resolve_preview_path(cwd, &params.path)?;
    let metadata = std::fs::metadata(&resolved)
        .map_err(|_| FilePreviewRouteError::NotFound("file not found".to_string()))?;
    if !metadata.is_file() {
        return Err(FilePreviewRouteError::Unsupported(
            "file preview only supports regular files".to_string(),
        ));
    }
    if metadata.len() > HTTP_FILE_PREVIEW_MAX_BYTES {
        return Err(FilePreviewRouteError::TooLarge(format!(
            "file is larger than the {} byte preview limit",
            HTTP_FILE_PREVIEW_MAX_BYTES
        )));
    }

    let bytes = std::fs::read(&resolved)
        .map_err(|_| FilePreviewRouteError::NotFound("file could not be read".to_string()))?;
    if bytes.contains(&0) {
        return Err(FilePreviewRouteError::Unsupported(
            "binary files are not supported in browser preview".to_string(),
        ));
    }
    let content = String::from_utf8(bytes).map_err(|_| {
        FilePreviewRouteError::Unsupported(
            "file is not valid UTF-8 text and cannot be previewed".to_string(),
        )
    })?;

    Ok(json!({
        "tab_id": params.tab,
        "pane_id": params.pane,
        "path": resolved.to_string_lossy(),
        "display_path": display_preview_path(&base, &resolved),
        "cwd": base.to_string_lossy(),
        "line": params.line,
        "col": params.col,
        "size_bytes": metadata.len(),
        "max_bytes": HTTP_FILE_PREVIEW_MAX_BYTES,
        "truncated": false,
        "content": content,
    }))
}

async fn build_file_stat_from_pane(
    pane: &Value,
    params: &FilePreviewQuery,
) -> Result<Value, FilePreviewRouteError> {
    let remote_shell = pane
        .get("remote_shell")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cwd_host = pane.get("cwd_host").and_then(Value::as_str);
    // A pane whose OSC7 host is this machine's own hostname is local; mirror the
    // GTK terminal, which resolves such references locally via `is_remote_host`.
    // Without this, ordinary local panes (OSC7 emits the hostname) would be
    // rejected as remote and browser file checks would never run.
    let has_remote_host = crate::terminal::is_remote_host(cwd_host);
    if remote_shell || has_remote_host {
        return Err(FilePreviewRouteError::Unsupported(
            "file stat is only available for local panes".to_string(),
        ));
    }

    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| FilePreviewRouteError::BadRequest("pane cwd is unknown".to_string()))?;

    match resolve_preview_path(cwd, &params.path) {
        Ok((base, resolved)) => {
            let metadata = std::fs::metadata(&resolved)
                .map_err(|_| FilePreviewRouteError::NotFound("file not found".to_string()))?;
            Ok(file_stat_response(
                params,
                &base,
                &resolved,
                true,
                metadata.is_file(),
                metadata.is_dir(),
            ))
        }
        Err(FilePreviewRouteError::NotFound(error)) if error == "file not found" => {
            let (base, resolved) = resolve_missing_stat_path(cwd, &params.path)?;
            Ok(file_stat_response(
                params, &base, &resolved, false, false, false,
            ))
        }
        Err(error) => Err(error),
    }
}

fn file_stat_response(
    params: &FilePreviewQuery,
    base: &Path,
    resolved: &Path,
    exists: bool,
    is_file: bool,
    is_dir: bool,
) -> Value {
    json!({
        "tab_id": params.tab,
        "pane_id": params.pane,
        "path": resolved.to_string_lossy(),
        "display_path": display_preview_path(base, resolved),
        "cwd": base.to_string_lossy(),
        "exists": exists,
        "is_file": is_file,
        "is_dir": is_dir,
    })
}

fn resolve_preview_path(
    cwd: &str,
    raw_path: &str,
) -> Result<(PathBuf, PathBuf), FilePreviewRouteError> {
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return Err(FilePreviewRouteError::BadRequest(
            "file path is required".to_string(),
        ));
    }

    let base = Path::new(cwd)
        .canonicalize()
        .map_err(|_| FilePreviewRouteError::NotFound("pane cwd does not exist".to_string()))?;
    let requested = Path::new(raw_path);
    if requested
        .components()
        .any(|component| matches!(component, Component::Prefix(_)))
    {
        return Err(FilePreviewRouteError::BadRequest(
            "unsupported file path".to_string(),
        ));
    }

    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base.join(requested)
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|_| FilePreviewRouteError::NotFound("file not found".to_string()))?;
    if !resolved.starts_with(&base) {
        return Err(FilePreviewRouteError::Forbidden(
            "file preview is limited to the pane cwd".to_string(),
        ));
    }
    Ok((base, resolved))
}

fn resolve_missing_stat_path(
    cwd: &str,
    raw_path: &str,
) -> Result<(PathBuf, PathBuf), FilePreviewRouteError> {
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return Err(FilePreviewRouteError::BadRequest(
            "file path is required".to_string(),
        ));
    }

    let base = Path::new(cwd)
        .canonicalize()
        .map_err(|_| FilePreviewRouteError::NotFound("pane cwd does not exist".to_string()))?;
    let requested = Path::new(raw_path);
    if requested
        .components()
        .any(|component| matches!(component, Component::Prefix(_)))
    {
        return Err(FilePreviewRouteError::BadRequest(
            "unsupported file path".to_string(),
        ));
    }

    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base.join(requested)
    };
    let normalized = normalize_stat_candidate(&candidate)?;
    if !normalized.starts_with(&base) {
        return Err(FilePreviewRouteError::Forbidden(
            "file stat is limited to the pane cwd".to_string(),
        ));
    }

    let mut ancestor = normalized.parent();
    while let Some(parent) = ancestor {
        if std::fs::metadata(parent).is_ok() {
            let resolved_parent = parent
                .canonicalize()
                .map_err(|_| FilePreviewRouteError::NotFound("file not found".to_string()))?;
            if !resolved_parent.starts_with(&base) {
                return Err(FilePreviewRouteError::Forbidden(
                    "file stat is limited to the pane cwd".to_string(),
                ));
            }
            return Ok((base, normalized));
        }
        ancestor = parent.parent();
    }

    Err(FilePreviewRouteError::NotFound(
        "file not found".to_string(),
    ))
}

fn normalize_stat_candidate(candidate: &Path) -> Result<PathBuf, FilePreviewRouteError> {
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(_) => {
                return Err(FilePreviewRouteError::BadRequest(
                    "unsupported file path".to_string(),
                ));
            }
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(FilePreviewRouteError::Forbidden(
                        "file stat is limited to the pane cwd".to_string(),
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn display_preview_path(base: &Path, resolved: &Path) -> String {
    resolved
        .strip_prefix(base)
        .ok()
        .and_then(|path| path.to_str())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| resolved.to_str().unwrap_or(""))
        .to_string()
}

async fn control_send_keys(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<SendKeysRequest>,
) -> Result<Json<Value>, ControlRouteError> {
    check_auth(&headers, &state.auth_token)?;
    check_control_enabled(&state)?;
    send_control_action(
        &state,
        HttpControlAction::SendKeys {
            tab: payload.tab,
            pane: payload.pane,
            keys: payload.keys,
        },
    )
    .await
}

async fn control_run_in_pane(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<RunInPaneRequest>,
) -> Result<Json<Value>, ControlRouteError> {
    check_auth(&headers, &state.auth_token)?;
    check_control_enabled(&state)?;
    send_control_action(
        &state,
        HttpControlAction::RunInPane {
            tab: payload.tab,
            pane: payload.pane,
            command: payload.command,
        },
    )
    .await
}

async fn control_switch_tab(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<SwitchTabRequest>,
) -> Result<Json<Value>, ControlRouteError> {
    check_auth(&headers, &state.auth_token)?;
    check_control_enabled(&state)?;
    send_control_action(&state, HttpControlAction::SwitchTab { tab: payload.tab }).await
}

async fn control_create_tab(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<CreateTabRequest>,
) -> Result<Json<Value>, ControlRouteError> {
    check_auth(&headers, &state.auth_token)?;
    check_control_enabled(&state)?;
    send_control_action(
        &state,
        HttpControlAction::CreateTab {
            name: payload.name,
            working_dir: payload.working_dir,
            command: payload.command,
        },
    )
    .await
}

async fn control_split_pane(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<SplitPaneRequest>,
) -> Result<Json<Value>, ControlRouteError> {
    check_auth(&headers, &state.auth_token)?;
    check_control_enabled(&state)?;
    send_control_action(
        &state,
        HttpControlAction::SplitPane {
            tab: payload.tab,
            direction: payload.direction,
            command: payload.command,
            working_dir: payload.working_dir,
        },
    )
    .await
}

async fn get_agent_sessions(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let live_bindings = query_agent_bindings(&state).await?;
    let snapshot = state.agent_session_catalog.snapshot(live_bindings).await;
    let data = serde_json::to_value(snapshot).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

fn required_state_field(data: &Value, field: &str) -> Result<Value, StatusCode> {
    data.get(field)
        .filter(|value| !value.is_null())
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn required_nullable_state_field(data: &Value, field: &str) -> Result<Value, StatusCode> {
    data.get(field)
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_workspaces(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let data = query_state_projection(&state, StateProjection::Workspaces).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

async fn get_tabs(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let data = query_state_projection(&state, StateProjection::Tabs).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

async fn get_panes(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let data = query_state_projection(&state, StateProjection::Panes).await?;
    Ok(Json(json!({ "ok": true, "data": data })))
}

async fn get_sessions(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&headers, &state.auth_token)?;
    let data = query_state_snapshot(&state).await?;
    let session_name = required_nullable_state_field(&data, "session_name")?;
    let active_workspace = required_state_field(&data, "active_workspace")?;
    let active_tab = required_state_field(&data, "active_tab")?;
    let dashboard = required_state_field(&data, "dashboard")?;
    let detached_sessions = required_state_field(&data, "detached_sessions")?;
    Ok(Json(json!({
        "ok": true,
        "data": {
            "session_name": session_name,
            "active_workspace": active_workspace,
            "active_tab": active_tab,
            "dashboard": dashboard,
            "detached_sessions": detached_sessions,
        }
    })))
}

async fn events_ws(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(auth): Query<WebSocketAuthQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    check_ws_auth(&headers, auth.token.as_deref(), &state.auth_token)?;
    let rx = state.event_broadcast.subscribe();
    Ok(ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let msg = Message::text(event.to_string());
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    // Notify the client that events were lost, then continue
                    let msg = Message::text(
                        serde_json::json!({
                            "event_type": "_lagged",
                            "skipped": skipped,
                            "resnapshot_required": true,
                        })
                        .to_string(),
                    );
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    }))
}

async fn spa_fallback(State(state): State<HttpState>, uri: Uri) -> Response {
    serve_web_request(&state, uri.path())
}

// ── Server startup ──

fn build_router_with_web_asset_candidates(
    bridge: BridgeSender,
    auth_token: String,
    event_broadcast: broadcast::Sender<Value>,
    web_asset_candidates: Vec<PathBuf>,
) -> Router {
    build_router_with_web_asset_candidates_and_catalog(
        bridge,
        auth_token,
        event_broadcast,
        web_asset_candidates,
        crate::agent_sessions::default_catalog(),
        false,
    )
}

fn build_router_with_web_asset_candidates_and_catalog(
    bridge: BridgeSender,
    auth_token: String,
    event_broadcast: broadcast::Sender<Value>,
    web_asset_candidates: Vec<PathBuf>,
    agent_session_catalog: Arc<crate::agent_sessions::AgentSessionCatalog>,
    control_enabled: bool,
) -> Router {
    build_router_with_web_asset_candidates_catalog_and_dirty(
        bridge,
        auth_token,
        event_broadcast,
        web_asset_candidates,
        agent_session_catalog,
        control_enabled,
        true,
        Arc::new(PaneDirtyRegistry::default()),
    )
}

/// A per-router runtime identifier stamped onto every remote protocol frame.
fn generate_runtime_id() -> String {
    crate::pty_broker::BrokerEpoch::new()
        .map(|epoch| epoch.to_string())
        .unwrap_or_else(|_| "00000000-0000-4000-8000-000000000000".to_string())
}

#[allow(clippy::too_many_arguments)] // Router assembly threads explicit runtime dependencies.
fn build_router_with_web_asset_candidates_catalog_and_dirty(
    bridge: BridgeSender,
    auth_token: String,
    event_broadcast: broadcast::Sender<Value>,
    web_asset_candidates: Vec<PathBuf>,
    agent_session_catalog: Arc<crate::agent_sessions::AgentSessionCatalog>,
    control_enabled: bool,
    bind_is_loopback: bool,
    pane_dirty: Arc<PaneDirtyRegistry>,
) -> Router {
    build_router_with_web_asset_candidates_catalog_dirty_and_history(
        bridge,
        auth_token,
        event_broadcast,
        web_asset_candidates,
        agent_session_catalog,
        control_enabled,
        bind_is_loopback,
        pane_dirty,
        crate::history::HistoryReader::disabled(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_router_with_web_asset_candidates_catalog_dirty_and_history(
    bridge: BridgeSender,
    auth_token: String,
    event_broadcast: broadcast::Sender<Value>,
    web_asset_candidates: Vec<PathBuf>,
    agent_session_catalog: Arc<crate::agent_sessions::AgentSessionCatalog>,
    control_enabled: bool,
    bind_is_loopback: bool,
    pane_dirty: Arc<PaneDirtyRegistry>,
    history_reader: crate::history::HistoryReader,
) -> Router {
    let pane_snapshotter = default_pane_snapshotter(bridge.clone());
    let pane_snapshot_slots = Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS));
    let pane_attach_hub = PaneAttachHub::new(
        bridge.clone(),
        Arc::clone(&pane_snapshotter),
        Arc::clone(&pane_snapshot_slots),
        Arc::clone(&pane_dirty),
        HTTP_PANE_ATTACH_POLL_INTERVAL,
    );
    let state = HttpState {
        bridge,
        auth_token: Arc::new(auth_token),
        control_enabled,
        bind_is_loopback,
        runtime_id: Arc::new(generate_runtime_id()),
        agent_session_catalog,
        event_broadcast,
        web_asset_candidates: Arc::new(web_asset_candidates),
        pane_snapshotter,
        pane_attach_slots: Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_CONNECTIONS)),
        pane_snapshot_slots,
        pane_attach_hub,
        history_reader,
    };

    build_router_with_state(state)
}

pub fn build_router(
    bridge: BridgeSender,
    auth_token: String,
    event_broadcast: broadcast::Sender<Value>,
) -> Router {
    build_router_with_web_asset_candidates(
        bridge,
        auth_token,
        event_broadcast,
        web_asset_candidate_paths(),
    )
}

/// Build a router with explicit control/loopback gates for the PTY adapter
/// tests. Lives in the module body (not `mod tests`) so the `pty` submodule's
/// tests can reach it.
#[cfg(test)]
pub(super) fn test_router_with_gates(
    control_enabled: bool,
    bind_is_loopback: bool,
) -> (Router, BridgeReceiver) {
    let (bridge_tx, bridge_rx) = mpsc::channel(16);
    let (event_tx, _) = broadcast::channel(16);
    let app = build_router_with_web_asset_candidates_catalog_and_dirty(
        bridge_tx,
        "secret".to_string(),
        event_tx,
        Vec::new(),
        crate::agent_sessions::default_catalog(),
        control_enabled,
        bind_is_loopback,
        Arc::new(PaneDirtyRegistry::default()),
    );
    (app, bridge_rx)
}

fn build_router_with_state(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/runtime-identity", get(get_runtime_identity))
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/agent-sessions", get(get_agent_sessions))
        .route("/api/v1/sessions", get(get_sessions))
        .route("/api/v1/workspaces", get(get_workspaces))
        .route("/api/v1/tabs", get(get_tabs))
        .route("/api/v1/panes", get(get_panes))
        .route("/api/v1/file-preview", get(file_preview))
        .route("/api/v1/file-stat", get(file_stat))
        .route("/api/v1/control/send-keys", post(control_send_keys))
        .route("/api/v1/control/run-in-pane", post(control_run_in_pane))
        .route("/api/v1/control/switch-tab", post(control_switch_tab))
        .route("/api/v1/control/create-tab", post(control_create_tab))
        .route("/api/v1/control/split-pane", post(control_split_pane))
        .route(
            "/api/v1/tabs/{tab_id}/panes/{pane_id}/attach",
            get(tab_pane_attach_ws),
        )
        .route(
            "/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws",
            get(tab_pane_attach_control_ws),
        )
        .route(
            "/api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws",
            get(tab_pane_pty_ws),
        )
        .route("/api/v1/panes/{pane_id}/attach", get(pane_attach_ws))
        .route("/api/v1/events", get(get_events))
        .route("/api/v1/history", get(get_history))
        .route("/api/v1/events/ws", get(events_ws))
        .fallback(spa_fallback)
        .with_state(state)
}

pub fn start_http_server(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_control_config(
        runtime_dir,
        config,
        &crate::config::HttpControlConfig::default(),
    )
}

pub fn start_http_server_with_control_config(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_web_asset_candidates_and_control_config(
        runtime_dir,
        config,
        control_config,
        web_asset_candidate_paths(),
    )
}

#[doc(hidden)]
pub fn start_http_server_with_control_config_and_dirty(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
    pane_dirty: Arc<PaneDirtyRegistry>,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_control_config_dirty_and_history(
        runtime_dir,
        config,
        control_config,
        pane_dirty,
        crate::history::HistoryReader::disabled(),
    )
}

#[doc(hidden)]
pub fn start_http_server_with_control_config_dirty_and_history(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
    pane_dirty: Arc<PaneDirtyRegistry>,
    history_reader: crate::history::HistoryReader,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_web_asset_candidates_control_config_dirty_and_history(
        runtime_dir,
        config,
        control_config,
        web_asset_candidate_paths(),
        pane_dirty,
        history_reader,
    )
}

#[doc(hidden)]
pub fn start_http_server_with_web_asset_candidates(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    web_asset_candidates: Vec<PathBuf>,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_web_asset_candidates_and_control_config(
        runtime_dir,
        config,
        &crate::config::HttpControlConfig::default(),
        web_asset_candidates,
    )
}

#[doc(hidden)]
pub fn start_http_server_with_web_asset_candidates_and_control_config(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
    web_asset_candidates: Vec<PathBuf>,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_web_asset_candidates_control_config_and_dirty(
        runtime_dir,
        config,
        control_config,
        web_asset_candidates,
        Arc::new(PaneDirtyRegistry::default()),
    )
}

#[doc(hidden)]
pub fn start_http_server_with_web_asset_candidates_control_config_and_dirty(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
    web_asset_candidates: Vec<PathBuf>,
    pane_dirty: Arc<PaneDirtyRegistry>,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    start_http_server_with_web_asset_candidates_control_config_dirty_and_history(
        runtime_dir,
        config,
        control_config,
        web_asset_candidates,
        pane_dirty,
        crate::history::HistoryReader::disabled(),
    )
}

#[doc(hidden)]
pub fn start_http_server_with_web_asset_candidates_control_config_dirty_and_history(
    runtime_dir: &Path,
    config: &crate::config::HttpConfig,
    control_config: &crate::config::HttpControlConfig,
    web_asset_candidates: Vec<PathBuf>,
    pane_dirty: Arc<PaneDirtyRegistry>,
    history_reader: crate::history::HistoryReader,
) -> Option<(BridgeReceiver, broadcast::Sender<Value>)> {
    if !config.enabled {
        return None;
    }

    let addr = match resolve_bind_addr(config) {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("taarof: HTTP API disabled: {error}");
            return None;
        }
    };

    let token = match generate_token() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("taarof: failed to generate HTTP auth token: {error}");
            return None;
        }
    };
    let token_file = token_path(runtime_dir);

    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("taarof: HTTP bind failed on {addr}: {error}");
            return None;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("taarof: failed to configure HTTP listener on {addr}: {error}");
        return None;
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("taarof: failed to create tokio runtime for HTTP server: {error}");
            return None;
        }
    };

    if let Err(error) = write_token_file(&token_file, &token) {
        eprintln!(
            "taarof: HTTP API disabled: failed to write auth token {}: {error}",
            token_file.display()
        );
        return None;
    }

    let (bridge_tx, bridge_rx) = mpsc::channel::<HttpBridgeRequest>(HTTP_BRIDGE_CAPACITY);
    let (event_tx, _) = broadcast::channel::<Value>(256);
    let control_enabled = http_control_enabled_for_bind(control_config, addr);
    if control_config.enabled && !control_enabled {
        eprintln!(
            "taarof: HTTP control API disabled: [http_control].enabled requires a loopback bind"
        );
    }

    let app = build_router_with_web_asset_candidates_catalog_dirty_and_history(
        bridge_tx,
        token.clone(),
        event_tx.clone(),
        web_asset_candidates,
        crate::agent_sessions::default_catalog(),
        control_enabled,
        addr.ip().is_loopback(),
        pane_dirty,
        history_reader,
    );

    std::thread::spawn(move || {
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("taarof: failed to adopt HTTP listener on {addr}: {error}");
                    return;
                }
            };
            axum::serve(listener, app).await.ok();
        });
    });

    warn_on_non_loopback_bind(addr);
    eprintln!("taarof: HTTP API at http://{addr}");
    eprintln!("taarof: auth token written to {}", token_file.display());

    Some((bridge_rx, event_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    // missing_web_dist_response now lives in the web_assets submodule;
    // the 503-pathless-body tests below still live here. The constants
    // already come in via `super::*`, so only the function needs importing.
    use super::web_assets::missing_web_dist_response;
    use crate::{seed_headless_terminal_tab, AppState, HeadlessPaneSeed};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use futures_util::{SinkExt, StreamExt};
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as TungsteniteMessage};
    use tower::ServiceExt;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "taarof-http-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn test_router(token: &str) -> (Router, BridgeReceiver) {
        test_router_with_control(token, false)
    }

    fn test_router_with_control(token: &str, control_enabled: bool) -> (Router, BridgeReceiver) {
        let (bridge_tx, bridge_rx) = mpsc::channel(8);
        let (event_tx, _) = broadcast::channel(16);
        let app = build_router_with_web_asset_candidates_and_catalog(
            bridge_tx,
            token.to_string(),
            event_tx,
            Vec::new(),
            Arc::new(crate::agent_sessions::AgentSessionCatalog::with_scanner(
                Arc::new(FixedAgentSessionScanner::empty()),
            )),
            control_enabled,
        );
        (app, bridge_rx)
    }

    fn test_http_state(
        token: &str,
        bridge: BridgeSender,
        pane_snapshotter: Arc<PaneSnapshotter>,
        pane_attach_poll_interval: Duration,
    ) -> HttpState {
        let (event_tx, _) = broadcast::channel(16);
        let pane_snapshot_slots = Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS));
        let pane_dirty = Arc::new(PaneDirtyRegistry::default());
        let pane_attach_hub = PaneAttachHub::new(
            bridge.clone(),
            Arc::clone(&pane_snapshotter),
            Arc::clone(&pane_snapshot_slots),
            Arc::clone(&pane_dirty),
            pane_attach_poll_interval,
        );
        HttpState {
            bridge,
            auth_token: Arc::new(token.to_string()),
            control_enabled: false,
            bind_is_loopback: true,
            runtime_id: Arc::new(generate_runtime_id()),
            agent_session_catalog: Arc::new(
                crate::agent_sessions::AgentSessionCatalog::with_scanner(Arc::new(
                    FixedAgentSessionScanner::empty(),
                )),
            ),
            event_broadcast: event_tx,
            web_asset_candidates: Arc::new(Vec::new()),
            pane_snapshotter,
            pane_attach_slots: Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_CONNECTIONS)),
            pane_snapshot_slots,
            pane_attach_hub,
            history_reader: crate::history::HistoryReader::disabled(),
        }
    }

    fn test_http_state_with_web_assets(web_asset_candidates: Vec<PathBuf>) -> HttpState {
        let (bridge_tx, _) = mpsc::channel(8);
        let (event_tx, _) = broadcast::channel(16);
        let pane_snapshotter = default_pane_snapshotter(bridge_tx.clone());
        let pane_snapshot_slots = Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS));
        let pane_dirty = Arc::new(PaneDirtyRegistry::default());
        let pane_attach_hub = PaneAttachHub::new(
            bridge_tx.clone(),
            Arc::clone(&pane_snapshotter),
            Arc::clone(&pane_snapshot_slots),
            Arc::clone(&pane_dirty),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        HttpState {
            bridge: bridge_tx.clone(),
            auth_token: Arc::new("secret".to_string()),
            control_enabled: false,
            bind_is_loopback: true,
            runtime_id: Arc::new(generate_runtime_id()),
            agent_session_catalog: Arc::new(
                crate::agent_sessions::AgentSessionCatalog::with_scanner(Arc::new(
                    FixedAgentSessionScanner::empty(),
                )),
            ),
            event_broadcast: event_tx,
            web_asset_candidates: Arc::new(web_asset_candidates),
            pane_snapshotter,
            pane_attach_slots: Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_CONNECTIONS)),
            pane_snapshot_slots,
            pane_attach_hub,
            history_reader: crate::history::HistoryReader::disabled(),
        }
    }

    #[derive(Clone, Debug)]
    struct ExpectedSnapshotCall {
        target: PaneAttachTarget,
        preserve_ansi: bool,
        snapshot: PaneSnapshot,
    }

    fn scripted_snapshotter(scripted: Vec<ExpectedSnapshotCall>) -> Arc<PaneSnapshotter> {
        let scripted = VecDeque::from(scripted);
        let fallback = scripted
            .back()
            .cloned()
            .expect("scripted snapshotter needs at least one call");
        let scripted = Arc::new(Mutex::new(scripted));
        Arc::new(move |target, preserve_ansi| {
            let scripted = Arc::clone(&scripted);
            let fallback = fallback.clone();
            Box::pin(async move {
                let mut scripted = scripted.lock().expect("snapshot script lock should hold");
                let expected = scripted.pop_front();
                let expected = expected.unwrap_or_else(|| fallback.clone());
                if expected.target != target || expected.preserve_ansi != preserve_ansi {
                    return Err(format!(
                        "unexpected pane snapshot request: expected target={:?} preserve_ansi={}, got target={:?} preserve_ansi={}",
                        expected.target, expected.preserve_ansi, target, preserve_ansi
                    ));
                }
                Ok(expected.snapshot)
            })
        })
    }

    fn counting_scripted_snapshotter(
        scripted: Vec<ExpectedSnapshotCall>,
        count: Arc<AtomicUsize>,
    ) -> Arc<PaneSnapshotter> {
        let scripted = VecDeque::from(scripted);
        let fallback = scripted
            .back()
            .cloned()
            .expect("scripted snapshotter needs at least one call");
        let scripted = Arc::new(Mutex::new(scripted));
        Arc::new(move |target, preserve_ansi| {
            let scripted = Arc::clone(&scripted);
            let fallback = fallback.clone();
            let count = Arc::clone(&count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                let mut scripted = scripted.lock().expect("snapshot script lock should hold");
                let expected = scripted.pop_front();
                let expected = expected.unwrap_or_else(|| fallback.clone());
                if expected.target != target || expected.preserve_ansi != preserve_ansi {
                    return Err(format!(
                        "unexpected pane snapshot request: expected target={:?} preserve_ansi={}, got target={:?} preserve_ansi={}",
                        expected.target, expected.preserve_ansi, target, preserve_ansi
                    ));
                }
                Ok(expected.snapshot)
            })
        })
    }

    fn test_pane_attach_hub(
        bridge: BridgeSender,
        pane_snapshotter: Arc<PaneSnapshotter>,
        dirty: Arc<PaneDirtyRegistry>,
        poll_interval: Duration,
        liveness_interval: Duration,
    ) -> Arc<PaneAttachHub> {
        PaneAttachHub::new_with_intervals(
            bridge,
            pane_snapshotter,
            Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS)),
            dirty,
            poll_interval,
            liveness_interval,
            Duration::from_millis(40),
        )
    }

    fn vte_attach_target(tab_id: u32, pane_id: u32) -> PaneAttachTarget {
        PaneAttachTarget {
            tab_id,
            pane_id,
            kind: PaneAttachKind::Vte,
        }
    }

    fn tmux_attach_target(tab_id: u32, pane_id: u32) -> PaneAttachTarget {
        PaneAttachTarget {
            tab_id,
            pane_id,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        }
    }

    async fn wait_for_hub_replace(
        rx: &mut tokio::sync::watch::Receiver<PaneAttachUpdate>,
    ) -> (PaneAttachTarget, PaneSnapshot) {
        tokio::time::timeout(Duration::from_millis(150), rx.changed())
            .await
            .expect("hub update should arrive")
            .expect("hub update channel should stay open");
        match rx.borrow_and_update().clone() {
            PaneAttachUpdate::Replace { target, snapshot } => (target, snapshot),
            other => panic!("expected replace update, got {other:?}"),
        }
    }

    struct FixedAgentSessionScanner {
        discovery: crate::agent_sessions::AgentSessionDiscovery,
    }

    impl FixedAgentSessionScanner {
        fn empty() -> Self {
            Self {
                discovery: crate::agent_sessions::AgentSessionDiscovery {
                    providers: Vec::new(),
                    sessions: Vec::new(),
                    remote_hosts: Vec::new(),
                },
            }
        }
    }

    impl crate::agent_sessions::AgentSessionScanner for FixedAgentSessionScanner {
        fn scan(&self) -> crate::agent_sessions::AgentSessionDiscovery {
            self.discovery.clone()
        }
    }

    fn spawn_pane_attach_responder(
        rx: BridgeReceiver,
        scripted: Vec<PaneAttachLookup>,
        fallback: PaneAttachLookup,
    ) {
        let responses = Arc::new(Mutex::new(VecDeque::from(scripted)));
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(request) = rx.recv().await {
                match request {
                    HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(stub_state_snapshot());
                    }
                    HttpBridgeRequest::QueryStateProjection { projection, reply } => {
                        let snapshot = stub_state_snapshot();
                        let _ = reply.send(state_snapshot_projection(&snapshot, projection));
                    }
                    HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({"schema": "taarof.events.v1"}));
                    }
                    HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let next = {
                            let mut responses =
                                responses.lock().expect("lookup script lock should hold");
                            responses.pop_front().unwrap_or_else(|| fallback.clone())
                        };
                        let _ = reply.send(next);
                    }
                    HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(PtyAdapterResolution::NotFound);
                    }
                    HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::EmitEvent { .. } => {}
                    HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    }

    async fn spawn_ws_test_server(state: HttpState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener should bind");
        let addr = listener
            .local_addr()
            .expect("test listener should have local addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, build_router_with_state(state))
                .await
                .expect("test axum server should run");
        });
        (addr, server)
    }

    async fn connect_pane_attach_socket(
        addr: SocketAddr,
        pane_id: u32,
        token: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://{addr}/api/v1/panes/{pane_id}/attach?token={token}");
        let (socket, response) = connect_async(url)
            .await
            .expect("websocket client should connect");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    async fn connect_pane_control_socket(
        addr: SocketAddr,
        tab_id: u32,
        pane_id: u32,
        token: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url =
            format!("ws://{addr}/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws?token={token}");
        let (socket, response) = connect_async(url)
            .await
            .expect("websocket client should connect");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        socket
    }

    /// Test fixture for the pane-attach WebSocket route. Wires together:
    ///   * a fresh `HttpState` with a scripted snapshotter and poll interval
    ///   * a bridge-driven `PaneAttachLookup` responder consuming the
    ///     caller-supplied lookup script + fallback
    ///   * an axum server bound to a random loopback port
    ///
    /// The fixture replaces the 30-line boilerplate that the four
    /// `pane_attach_route_*` tests previously inlined (HTTP state
    /// construction, responder spawning, server bind, addr wiring). Each
    /// test now states only the snapshots, lookups, and the assertion
    /// that matters to it. Call `state_override` (or
    /// `exhausted_attach_slots`) for the one-off tweaks the rejected
    /// upgrade test needs.
    struct PaneAttachFixture {
        addr: SocketAddr,
        server: tokio::task::JoinHandle<()>,
    }

    impl PaneAttachFixture {
        /// Build the full fixture. `lookups` is consumed in order by the
        /// bridge responder; once exhausted, `fallback_lookup` is used
        /// for every subsequent lookup.
        async fn new(
            token: &str,
            snapshots: Vec<ExpectedSnapshotCall>,
            lookups: Vec<PaneAttachLookup>,
            fallback_lookup: PaneAttachLookup,
        ) -> Self {
            let (bridge_tx, bridge_rx) = mpsc::channel(8);
            let state = test_http_state(
                token,
                bridge_tx,
                scripted_snapshotter(snapshots),
                Duration::from_millis(20),
            );
            spawn_pane_attach_responder(bridge_rx, lookups, fallback_lookup);
            let (addr, server) = spawn_ws_test_server(state).await;
            Self { addr, server }
        }

        /// Like [`Self::new`] but with a closure that mutates the
        /// `HttpState` after it is built. Used by the
        /// exhausted-attach-slots test to swap in a zero-permit
        /// semaphore.
        async fn new_with_state_override(
            token: &str,
            snapshots: Vec<ExpectedSnapshotCall>,
            lookups: Vec<PaneAttachLookup>,
            fallback_lookup: PaneAttachLookup,
            state_override: impl FnOnce(&mut HttpState),
        ) -> Self {
            let (bridge_tx, bridge_rx) = mpsc::channel(8);
            let mut state = test_http_state(
                token,
                bridge_tx,
                scripted_snapshotter(snapshots),
                Duration::from_millis(20),
            );
            state_override(&mut state);
            spawn_pane_attach_responder(bridge_rx, lookups, fallback_lookup);
            let (addr, server) = spawn_ws_test_server(state).await;
            Self { addr, server }
        }

        /// Pre-built state override for the exhausted-attach-slots case.
        fn exhausted_attach_slots(state: &mut HttpState) {
            state.pane_attach_slots = Arc::new(Semaphore::new(0));
        }

        async fn connect_pane(
            &self,
            pane_id: u32,
            token: &str,
        ) -> tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        > {
            connect_pane_attach_socket(self.addr, pane_id, token).await
        }

        async fn shutdown(self) {
            self.server.abort();
            let _ = self.server.await;
        }
    }

    #[tokio::test]
    async fn pane_attach_hub_shares_one_source_across_clients() {
        let target = vte_attach_target(1, 7);
        let key = PaneKey::from(&target);
        let dirty = Arc::new(PaneDirtyRegistry::default());
        let capture_count = Arc::new(AtomicUsize::new(0));
        let snapshotter = counting_scripted_snapshotter(
            vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "updated".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ],
            Arc::clone(&capture_count),
        );
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        spawn_pane_attach_responder(
            bridge_rx,
            Vec::new(),
            PaneAttachLookup::Attachable(target.clone()),
        );
        let hub = test_pane_attach_hub(
            bridge_tx,
            snapshotter,
            Arc::clone(&dirty),
            Duration::from_millis(10),
            Duration::from_secs(60),
        );

        let mut first = hub
            .subscribe(target.clone())
            .await
            .expect("first subscriber should attach");
        let mut second = hub
            .subscribe(target.clone())
            .await
            .expect("second subscriber should attach");
        assert_eq!(capture_count.load(Ordering::SeqCst), 1);
        assert_eq!(hub.subscriber_count(key), Some(2));

        first.rx.borrow_and_update();
        second.rx.borrow_and_update();
        dirty.mark_dirty(target.tab_id, target.pane_id);

        let first_update = wait_for_hub_replace(&mut first.rx).await;
        let second_update = wait_for_hub_replace(&mut second.rx).await;

        assert_eq!(capture_count.load(Ordering::SeqCst), 2);
        assert_eq!(first_update, second_update);
        assert_eq!(first_update.1.output, "updated");
        assert_eq!(hub.source_count(), 1);
    }

    #[tokio::test]
    async fn pane_attach_hub_vte_idle_does_not_capture_text() {
        let target = vte_attach_target(1, 7);
        let dirty = Arc::new(PaneDirtyRegistry::default());
        let capture_count = Arc::new(AtomicUsize::new(0));
        let snapshotter = counting_scripted_snapshotter(
            vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready\nnext".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ],
            Arc::clone(&capture_count),
        );
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        spawn_pane_attach_responder(
            bridge_rx,
            Vec::new(),
            PaneAttachLookup::Attachable(target.clone()),
        );
        let hub = test_pane_attach_hub(
            bridge_tx,
            snapshotter,
            Arc::clone(&dirty),
            Duration::from_millis(10),
            Duration::from_millis(10),
        );

        let mut subscription = hub
            .subscribe(target.clone())
            .await
            .expect("subscriber should attach");
        subscription.rx.borrow_and_update();
        assert_eq!(capture_count.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(35)).await;
        assert_eq!(
            capture_count.load(Ordering::SeqCst),
            1,
            "VTE liveness ticks must not capture visible text while idle"
        );

        dirty.mark_dirty(target.tab_id, target.pane_id);
        let (_target, snapshot) = wait_for_hub_replace(&mut subscription.rx).await;
        assert_eq!(snapshot.output, "ready\nnext");
        assert_eq!(capture_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pane_attach_hub_tmux_polls_and_backs_off() {
        let target = tmux_attach_target(1, 7);
        let dirty = Arc::new(PaneDirtyRegistry::default());
        let capture_count = Arc::new(AtomicUsize::new(0));
        let snapshotter = counting_scripted_snapshotter(
            vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "updated".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "updated".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ],
            Arc::clone(&capture_count),
        );
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        spawn_pane_attach_responder(
            bridge_rx,
            Vec::new(),
            PaneAttachLookup::Attachable(target.clone()),
        );
        let hub = test_pane_attach_hub(
            bridge_tx,
            snapshotter,
            dirty,
            Duration::from_millis(10),
            Duration::from_secs(60),
        );

        let mut subscription = hub
            .subscribe(target.clone())
            .await
            .expect("subscriber should attach");
        subscription.rx.borrow_and_update();
        assert_eq!(capture_count.load(Ordering::SeqCst), 1);

        let (_target, snapshot) = wait_for_hub_replace(&mut subscription.rx).await;
        assert_eq!(snapshot.output, "updated");
        assert!(capture_count.load(Ordering::SeqCst) >= 2);

        let duplicate_replace =
            tokio::time::timeout(Duration::from_millis(35), subscription.rx.changed()).await;
        assert!(
            duplicate_replace.is_err(),
            "unchanged tmux poll should update the baseline/backoff without sending replace"
        );
        assert_eq!(hub.subscriber_count(PaneKey::from(&target)), Some(1));
        assert_eq!(
            hub.next_idle_interval(Duration::from_millis(10)),
            Duration::from_millis(20)
        );
        assert_eq!(
            hub.next_idle_interval(Duration::from_millis(40)),
            Duration::from_millis(40)
        );
    }

    #[tokio::test]
    async fn pane_attach_hub_releases_source_when_last_subscriber_drops() {
        let target = vte_attach_target(1, 7);
        let key = PaneKey::from(&target);
        let dirty = Arc::new(PaneDirtyRegistry::default());
        let capture_count = Arc::new(AtomicUsize::new(0));
        let snapshotter = counting_scripted_snapshotter(
            vec![ExpectedSnapshotCall {
                target: target.clone(),
                preserve_ansi: false,
                snapshot: PaneSnapshot {
                    output: "ready".into(),
                    width: 80,
                    height: 24,
                },
            }],
            Arc::clone(&capture_count),
        );
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        spawn_pane_attach_responder(
            bridge_rx,
            Vec::new(),
            PaneAttachLookup::Attachable(target.clone()),
        );
        let hub = test_pane_attach_hub(
            bridge_tx,
            snapshotter,
            Arc::clone(&dirty),
            Duration::from_millis(10),
            Duration::from_secs(60),
        );

        let first = hub
            .subscribe(target.clone())
            .await
            .expect("first subscriber should attach");
        let second = hub
            .subscribe(target.clone())
            .await
            .expect("second subscriber should attach");
        assert_eq!(hub.subscriber_count(key), Some(2));

        drop(first);
        assert_eq!(hub.subscriber_count(key), Some(1));
        drop(second);
        assert_eq!(hub.source_count(), 0);

        let captures_after_drop = capture_count.load(Ordering::SeqCst);
        dirty.mark_dirty(target.tab_id, target.pane_id);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(capture_count.load(Ordering::SeqCst), captures_after_drop);
    }

    #[tokio::test]
    async fn pane_attach_hub_two_clients_receive_byte_identical_replace_frames() {
        let target = vte_attach_target(1, 7);
        let dirty = Arc::new(PaneDirtyRegistry::default());
        let snapshotter = scripted_snapshotter(vec![
            ExpectedSnapshotCall {
                target: target.clone(),
                preserve_ansi: false,
                snapshot: PaneSnapshot {
                    output: "ready".into(),
                    width: 80,
                    height: 24,
                },
            },
            ExpectedSnapshotCall {
                target: target.clone(),
                preserve_ansi: false,
                snapshot: PaneSnapshot {
                    output: "updated".into(),
                    width: 80,
                    height: 24,
                },
            },
        ]);
        let (bridge_tx, bridge_rx) = mpsc::channel(16);
        spawn_pane_attach_responder(
            bridge_rx,
            Vec::new(),
            PaneAttachLookup::Attachable(target.clone()),
        );
        let hub = test_pane_attach_hub(
            bridge_tx,
            snapshotter,
            Arc::clone(&dirty),
            Duration::from_millis(10),
            Duration::from_secs(60),
        );

        let mut first = hub
            .subscribe(target.clone())
            .await
            .expect("first subscriber should attach");
        let mut second = hub
            .subscribe(target.clone())
            .await
            .expect("second subscriber should attach");
        first.rx.borrow_and_update();
        second.rx.borrow_and_update();

        dirty.mark_dirty(target.tab_id, target.pane_id);
        let (first_target, first_snapshot) = wait_for_hub_replace(&mut first.rx).await;
        let (second_target, second_snapshot) = wait_for_hub_replace(&mut second.rx).await;

        let first_replace = replace_frame_json(&first_target, &first_snapshot).to_string();
        let second_replace = replace_frame_json(&second_target, &second_snapshot).to_string();
        assert_eq!(first_replace, second_replace);

        let replace: Value =
            serde_json::from_str(&first_replace).expect("replace frame should be valid json");
        assert_eq!(replace["type"], serde_json::json!("replace"));
        assert_eq!(replace["encoding"], serde_json::json!("utf8"));
        assert_eq!(replace["payload"], serde_json::json!("updated"));
    }

    #[tokio::test]
    async fn health_endpoint_needs_no_auth() {
        let (app, rx) = test_router("secret");
        tokio::spawn(async move {
            let mut rx = rx;
            if let Some(HttpBridgeRequest::QueryHealth { reply }) = rx.recv().await {
                let _ = reply.send(serde_json::json!({
                    "state": "degraded",
                    "degraded": true,
                    "degraded_components": 1,
                    "runtime_probe": {
                        "state": "partial",
                        "process": { "state": "stale", "error": "must stay private" },
                        "ports": { "state": "ok" },
                    },
                }));
            }
        });
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("health body should be readable");
        let payload: Value = serde_json::from_slice(&body).expect("health body should be json");
        assert_eq!(payload["ok"], serde_json::json!(true));
        assert_eq!(payload["degraded"], serde_json::json!(true));
        assert_eq!(payload["state"], serde_json::json!("degraded"));
        assert_eq!(
            payload["runtime_probe"]["state"],
            serde_json::json!("partial")
        );
        assert_eq!(
            payload["runtime_probe"]["process_state"],
            serde_json::json!("stale")
        );
        assert_eq!(
            payload["runtime_probe"]["ports_state"],
            serde_json::json!("ok")
        );
        assert!(payload["runtime_probe"].get("error").is_none());
        assert!(
            payload.get("session_name").is_none(),
            "session_name must not leak to unauthenticated /health"
        );
        assert!(
            payload.get("health").is_none(),
            "internal health object must not leak to unauthenticated /health"
        );
    }

    #[tokio::test]
    async fn tab_scoped_pane_attach_route_sends_tab_id_to_bridge() {
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let state = test_http_state(
            "secret",
            bridge_tx.clone(),
            default_pane_snapshotter(bridge_tx),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        let (addr, server) = spawn_ws_test_server(state).await;
        let bridge_task = tokio::spawn(async move {
            match bridge_rx
                .recv()
                .await
                .expect("bridge should receive attach lookup")
            {
                HttpBridgeRequest::ResolvePaneAttach {
                    tab_id,
                    pane_id,
                    reply,
                } => {
                    assert_eq!(tab_id, Some(42));
                    assert_eq!(pane_id, 7);
                    let _ = reply.send(PaneAttachLookup::Unsupported);
                }
                _ => panic!("unexpected bridge request"),
            }
        });

        let url = format!("ws://{addr}/api/v1/tabs/42/panes/7/attach?token=secret");
        let error = connect_async(url)
            .await
            .expect_err("unsupported attach should reject websocket upgrade");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::CONFLICT);
            }
            other => panic!("unexpected websocket error: {other}"),
        }
        bridge_task.await.expect("bridge task should complete");
        server.abort();
    }

    #[tokio::test]
    async fn http_control_ws_rejects_when_control_disabled() {
        let (bridge_tx, _bridge_rx) = mpsc::channel(8);
        let state = test_http_state(
            "secret",
            bridge_tx.clone(),
            default_pane_snapshotter(bridge_tx),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        let (addr, server) = spawn_ws_test_server(state).await;
        let url = format!("ws://{addr}/api/v1/tabs/42/panes/7/control/ws?token=secret");
        let error = connect_async(url)
            .await
            .expect_err("disabled control should reject websocket upgrade");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
            other => panic!("unexpected websocket error: {other}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn http_control_ws_rejects_missing_auth_before_upgrade() {
        let (bridge_tx, _bridge_rx) = mpsc::channel(8);
        let mut state = test_http_state(
            "secret",
            bridge_tx.clone(),
            default_pane_snapshotter(bridge_tx),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        state.control_enabled = true;
        let (addr, server) = spawn_ws_test_server(state).await;
        let url = format!("ws://{addr}/api/v1/tabs/42/panes/7/control/ws");
        let error = connect_async(url)
            .await
            .expect_err("missing auth should reject websocket upgrade");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("unexpected websocket error: {other}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn http_control_ws_input_frames_dispatch_send_keys_action() {
        let target = vte_attach_target(42, 7);
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let mut state = test_http_state(
            "secret",
            bridge_tx,
            scripted_snapshotter(vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ]),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        state.control_enabled = true;
        let bridge_task = tokio::spawn(async move {
            for _ in 0..2 {
                match bridge_rx.recv().await {
                    Some(HttpBridgeRequest::ResolvePaneAttach {
                        tab_id,
                        pane_id,
                        reply,
                    }) => {
                        assert_eq!(tab_id, Some(42));
                        assert_eq!(pane_id, 7);
                        let _ = reply.send(PaneAttachLookup::Attachable(target.clone()));
                    }
                    _ => panic!("expected pane lookup request"),
                }
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::EmitEvent {
                    event_type,
                    payload,
                }) => {
                    assert_eq!(event_type, "http_pane_control_lifecycle");
                    assert_eq!(payload["lifecycle"], serde_json::json!("opened"));
                    assert_eq!(payload["tab_id"], serde_json::json!(42));
                    assert_eq!(payload["pane_id"], serde_json::json!(7));
                }
                _ => panic!("expected pane control lifecycle event"),
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert_eq!(
                        action,
                        HttpControlAction::SendKeys {
                            tab: Some("42".to_string()),
                            pane: 7,
                            keys: "abc\r".to_string(),
                        }
                    );
                    let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                }
                _ => panic!("expected send-keys control action"),
            }
        });

        let (addr, server) = spawn_ws_test_server(state).await;
        let mut socket = connect_pane_control_socket(addr, 42, 7, "secret").await;
        let first_frame = socket
            .next()
            .await
            .expect("control snapshot should arrive")
            .expect("snapshot frame should be ok")
            .into_text()
            .expect("snapshot frame should be text");
        let first_frame: Value =
            serde_json::from_str(&first_frame).expect("snapshot frame should be valid json");
        assert_eq!(first_frame["type"], serde_json::json!("snapshot"));

        socket
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "input",
                    "encoding": "utf8",
                    "payload": "abc\r",
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("input frame should send");

        bridge_task.await.expect("bridge task should complete");
        let _ = socket.send(TungsteniteMessage::Close(None)).await;
        server.abort();
    }

    #[tokio::test]
    async fn http_control_ws_resize_frames_dispatch_resize_action_and_ack() {
        let target = vte_attach_target(42, 7);
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let mut state = test_http_state(
            "secret",
            bridge_tx,
            scripted_snapshotter(vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ]),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        state.control_enabled = true;
        let bridge_task = tokio::spawn(async move {
            for _ in 0..2 {
                match bridge_rx.recv().await {
                    Some(HttpBridgeRequest::ResolvePaneAttach {
                        tab_id,
                        pane_id,
                        reply,
                    }) => {
                        assert_eq!(tab_id, Some(42));
                        assert_eq!(pane_id, 7);
                        let _ = reply.send(PaneAttachLookup::Attachable(target.clone()));
                    }
                    _ => panic!("expected pane lookup request"),
                }
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::EmitEvent {
                    event_type,
                    payload,
                }) => {
                    assert_eq!(event_type, "http_pane_control_lifecycle");
                    assert_eq!(payload["lifecycle"], serde_json::json!("opened"));
                    assert_eq!(payload["tab_id"], serde_json::json!(42));
                    assert_eq!(payload["pane_id"], serde_json::json!(7));
                }
                _ => panic!("expected pane control lifecycle event"),
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert_eq!(
                        action,
                        HttpControlAction::ResizePane {
                            tab: Some("42".to_string()),
                            pane: 7,
                            cols: 120,
                            rows: 30,
                        }
                    );
                    let _ = reply.send(Ok(serde_json::json!({
                        "ok": true,
                        "pane_id": 7,
                        "data": {
                            "supported": false,
                            "message": "backend pane resize is not supported yet",
                        },
                    })));
                }
                _ => panic!("expected resize control action"),
            }
        });

        let (addr, server) = spawn_ws_test_server(state).await;
        let mut socket = connect_pane_control_socket(addr, 42, 7, "secret").await;
        let _ = socket
            .next()
            .await
            .expect("control snapshot should arrive")
            .expect("snapshot frame should be ok");

        socket
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "resize",
                    "cols": 120,
                    "rows": 30,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("resize frame should send");
        let resize_frame = socket
            .next()
            .await
            .expect("resize ack should arrive")
            .expect("resize ack should be ok")
            .into_text()
            .expect("resize ack should be text");
        let resize_frame: Value =
            serde_json::from_str(&resize_frame).expect("resize ack should be valid json");
        assert_eq!(resize_frame["type"], serde_json::json!("resize"));
        assert_eq!(resize_frame["supported"], serde_json::json!(false));
        assert_eq!(resize_frame["cols"], serde_json::json!(120));
        assert_eq!(resize_frame["rows"], serde_json::json!(30));

        bridge_task.await.expect("bridge task should complete");
        let _ = socket.send(TungsteniteMessage::Close(None)).await;
        server.abort();
    }

    #[tokio::test]
    async fn http_control_ws_resize_dispatch_failure_keeps_socket_open() {
        let target = vte_attach_target(42, 7);
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let mut state = test_http_state(
            "secret",
            bridge_tx,
            scripted_snapshotter(vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ]),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        state.control_enabled = true;
        let bridge_task = tokio::spawn(async move {
            for _ in 0..2 {
                match bridge_rx.recv().await {
                    Some(HttpBridgeRequest::ResolvePaneAttach {
                        tab_id,
                        pane_id,
                        reply,
                    }) => {
                        assert_eq!(tab_id, Some(42));
                        assert_eq!(pane_id, 7);
                        let _ = reply.send(PaneAttachLookup::Attachable(target.clone()));
                    }
                    _ => panic!("expected pane lookup request"),
                }
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::EmitEvent { payload, .. }) => {
                    assert_eq!(payload["lifecycle"], serde_json::json!("opened"));
                }
                _ => panic!("expected pane control lifecycle event"),
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert_eq!(
                        action,
                        HttpControlAction::ResizePane {
                            tab: Some("42".to_string()),
                            pane: 7,
                            cols: 120,
                            rows: 30,
                        }
                    );
                    let _ = reply.send(Err("tmux resize-pane failed: missing pane".to_string()));
                }
                _ => panic!("expected resize control action"),
            }

            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert_eq!(
                        action,
                        HttpControlAction::SendKeys {
                            tab: Some("42".to_string()),
                            pane: 7,
                            keys: "abc".to_string(),
                        }
                    );
                    let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                }
                _ => panic!("expected follow-up send-keys control action"),
            }
        });

        let (addr, server) = spawn_ws_test_server(state).await;
        let mut socket = connect_pane_control_socket(addr, 42, 7, "secret").await;
        let _ = socket
            .next()
            .await
            .expect("control snapshot should arrive")
            .expect("snapshot frame should be ok");

        socket
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "resize",
                    "cols": 120,
                    "rows": 30,
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("resize frame should send");
        let resize_frame = socket
            .next()
            .await
            .expect("resize failure ack should arrive")
            .expect("resize failure ack should be ok")
            .into_text()
            .expect("resize failure ack should be text");
        let resize_frame: Value =
            serde_json::from_str(&resize_frame).expect("resize failure ack should be valid json");
        assert_eq!(resize_frame["type"], serde_json::json!("resize"));
        assert_eq!(resize_frame["supported"], serde_json::json!(false));
        assert_eq!(
            resize_frame["message"],
            serde_json::json!("tmux resize-pane failed: missing pane")
        );

        socket
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "input",
                    "encoding": "utf8",
                    "payload": "abc",
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("input should still send after resize failure");

        tokio::time::timeout(Duration::from_secs(1), bridge_task)
            .await
            .expect("bridge task should observe follow-up input")
            .expect("bridge task should complete");
        let _ = socket.send(TungsteniteMessage::Close(None)).await;
        server.abort();
    }

    #[tokio::test]
    async fn state_endpoint_rejects_missing_auth() {
        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(Request::get("/api/v1/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn runtime_identity_endpoint_requires_auth_and_returns_router_identity() {
        let (app, _rx) = test_router("secret");

        let unauthorized = app
            .clone()
            .oneshot(
                Request::get("/api/v1/runtime-identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                Request::get("/api/v1/runtime-identity")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("runtime identity body should be readable");
        let payload: Value =
            serde_json::from_slice(&body).expect("runtime identity body should be JSON");
        assert_eq!(payload["ok"], serde_json::json!(true));
        assert_eq!(
            payload["data"]["schema"],
            serde_json::json!("taarof.runtime-identity.v1")
        );
        assert_eq!(
            payload["data"]["session_name"],
            serde_json::json!("default")
        );
        let runtime_id = payload["data"]["runtime_id"]
            .as_str()
            .expect("runtime_id should be a string");
        assert_eq!(runtime_id.len(), 36);
        assert_eq!(runtime_id.matches('-').count(), 4);
    }

    #[tokio::test]
    async fn state_endpoint_rejects_wrong_token() {
        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(
                Request::get("/api/v1/state")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn state_endpoint_sends_bridge_request_with_valid_auth() {
        let (app, rx) = test_router("secret");

        tokio::spawn(async move {
            let mut rx = rx;
            if let Some(HttpBridgeRequest::QueryState { reply }) = rx.recv().await {
                let _ = reply.send(serde_json::json!({"schema": "taarof.state.v1"}));
            }
        });

        let response = app
            .oneshot(
                Request::get("/api/v1/state")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_endpoint_passes_query_params_through_bridge() {
        let (app, rx) = test_router("secret");

        tokio::spawn(async move {
            let mut rx = rx;
            if let Some(HttpBridgeRequest::QueryEvents {
                since_seq,
                limit,
                reply,
            }) = rx.recv().await
            {
                assert_eq!(since_seq, Some(42));
                assert_eq!(limit, Some(10));
                let _ = reply.send(serde_json::json!({"schema": "taarof.events.v1"}));
            }
        });

        let response = app
            .oneshot(
                Request::get("/api/v1/events?since_seq=42&limit=10")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn history_endpoint_queries_reader_without_gtk_bridge() {
        let dir = unique_temp_dir("history-endpoint");
        let path = dir.join("history.sqlite3");
        let config = crate::history::HistoryConfig {
            enabled: true,
            ..crate::history::HistoryConfig::default()
        };
        let (handle, reader) = crate::history::HistoryHandle::open_at(config, path);
        let event_ts = crate::events::unix_time_ms();
        handle.try_record_event(&crate::events::EventRecord {
            seq: 7,
            ts_unix_ms: event_ts,
            event_type: "session_started".to_string(),
            payload: serde_json::json!({"session_name": "http-test"}),
        });
        handle.flush().unwrap();

        let socket_request = serde_json::to_string(&serde_json::json!({
            "action": "query-history",
            "limit": 10,
            "record_type": "event",
            "text": "session",
            "order": "desc",
            "scan_budget": 50,
            "from_ts": event_ts.saturating_sub(60_000),
            "to_ts": event_ts.saturating_add(60_000),
        }))
        .unwrap();
        let socket_data =
            crate::socket::query_history_request_for_tests(&socket_request, &reader).unwrap();

        let (bridge, _bridge_rx) = mpsc::channel(1);
        let (event_tx, _) = broadcast::channel(1);
        let app = build_router_with_web_asset_candidates_catalog_dirty_and_history(
            bridge,
            "secret".to_string(),
            event_tx,
            Vec::new(),
            crate::agent_sessions::default_catalog(),
            false,
            true,
            Arc::new(PaneDirtyRegistry::default()),
            reader,
        );
        let response = app
            .oneshot(
                Request::get(format!(
                    "/api/v1/history?record_type=event&text=session&order=desc&scan_budget=50&from_ts={}&to_ts={}&limit=10",
                    event_ts.saturating_sub(60_000),
                    event_ts.saturating_add(60_000)
                ))
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["schema"], "taarof.history.v1");
        assert_eq!(json["data"]["records"][0]["source_seq"], 7);
        assert_eq!(json["data"], socket_data);
        drop(handle);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn history_endpoint_preserves_unavailable_error_envelope() {
        let (app, _bridge) = test_router("secret");
        let response = app
            .oneshot(
                Request::get("/api/v1/history?order=desc")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()));
    }

    fn file_preview_state(cwd: &Path, remote_shell: bool, cwd_host: Option<&str>) -> Value {
        serde_json::json!({
            "schema": "taarof.state.v1",
            "session_name": "test",
            "active_workspace": 1,
            "active_tab": 101,
            "dashboard": {},
            "detached_sessions": [],
            "workspaces": [{
                "id": 1,
                "name": "default",
                "tabs": [{
                    "tab_id": 101,
                    "name": "Shell",
                    "panes": [{
                        "pane_id": 7,
                        "shell_running": true,
                        "remote_shell": remote_shell,
                        "cwd": cwd.to_string_lossy(),
                        "cwd_host": cwd_host,
                    }]
                }]
            }]
        })
    }

    fn file_preview_request(path: &str) -> Request<Body> {
        Request::get(format!(
            "/api/v1/file-preview?tab=101&pane=7&path={path}&line=2&col=3"
        ))
        .header("authorization", "Bearer secret")
        .body(Body::empty())
        .expect("file preview request should build")
    }

    fn file_stat_request(path: &str) -> Request<Body> {
        file_stat_request_with_auth(path, Some("secret"))
    }

    fn file_stat_request_with_auth(path: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::get(format!("/api/v1/file-stat?tab=101&pane=7&path={path}"));
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::empty())
            .expect("file stat request should build")
    }

    #[tokio::test]
    async fn file_preview_returns_bounded_text_for_workspace_file() {
        let root = unique_temp_dir("file-preview-ok");
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("src dir should be created");
        std::fs::write(src.join("main.rs"), "fn main() {}\n").expect("file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));

        let response = app
            .oneshot(file_preview_request("src/main.rs"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("file preview response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("file preview response should be json");
        assert_eq!(payload["data"]["content"], "fn main() {}\n");
        assert_eq!(payload["data"]["display_path"], "src/main.rs");
        assert_eq!(payload["data"]["line"], 2);
        assert_eq!(payload["data"]["col"], 3);
    }

    #[tokio::test]
    async fn file_preview_rejects_traversal_outside_pane_cwd() {
        let root = unique_temp_dir("file-preview-traversal");
        let outside = root
            .parent()
            .expect("temp root should have parent")
            .join(format!("taarof-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "outside\n").expect("outside file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));

        let request_path = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let response = app
            .oneshot(file_preview_request(&request_path))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_file(outside);
    }

    #[tokio::test]
    async fn file_preview_allows_local_host_cwd_host() {
        // A local pane's OSC7 metadata reports this machine's own hostname in
        // cwd_host. That is still local (matches the GTK `is_local_host` rule),
        // so preview must succeed rather than being rejected as remote.
        let root = unique_temp_dir("file-preview-local-host");
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("file should be written");
        let local_host: String = glib::host_name().into();
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(
            rx,
            file_preview_state(&root, false, Some(local_host.as_str())),
        );

        let response = app.oneshot(file_preview_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("file preview response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("file preview response should be json");
        assert_eq!(payload["data"]["content"], "fn main() {}\n");
        assert_eq!(payload["data"]["display_path"], "main.rs");
    }

    #[tokio::test]
    async fn file_preview_allows_normalized_localhost_cwd_host() {
        let root = unique_temp_dir("file-preview-normalized-localhost");
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(
            rx,
            file_preview_state(&root, false, Some("  LOCALHOST  ")),
        );

        let response = app.oneshot(file_preview_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn file_preview_rejects_remote_panes() {
        let root = unique_temp_dir("file-preview-remote");
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, true, Some("dev.ts")));

        let response = app.oneshot(file_preview_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn file_stat_reports_existing_binary_file_without_reading_preview_content() {
        let root = unique_temp_dir("file-stat-binary");
        std::fs::write(root.join("blob.bin"), [0_u8, 159, 146, 150])
            .expect("binary file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));

        let response = app.oneshot(file_stat_request("blob.bin")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("file stat response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("file stat response should be json");
        assert_eq!(payload["data"]["exists"], true);
        assert_eq!(payload["data"]["is_file"], true);
        assert_eq!(payload["data"]["is_dir"], false);
        assert_eq!(payload["data"]["display_path"], "blob.bin");
        assert!(payload["data"].get("content").is_none());
    }

    #[tokio::test]
    async fn file_stat_allows_normalized_localhost_cwd_host() {
        let root = unique_temp_dir("file-stat-normalized-localhost");
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(
            rx,
            file_preview_state(&root, false, Some("  LOCALHOST  ")),
        );

        let response = app.oneshot(file_stat_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn file_stat_reports_existing_directory() {
        let root = unique_temp_dir("file-stat-dir");
        std::fs::create_dir_all(root.join("src")).expect("src dir should be created");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));

        let response = app.oneshot(file_stat_request("src")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("file stat response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("file stat response should be json");
        assert_eq!(payload["data"]["exists"], true);
        assert_eq!(payload["data"]["is_file"], false);
        assert_eq!(payload["data"]["is_dir"], true);
    }

    #[tokio::test]
    async fn file_stat_rejects_traversal_outside_pane_cwd() {
        let root = unique_temp_dir("file-stat-traversal");
        let outside = root
            .parent()
            .expect("temp root should have parent")
            .join(format!("taarof-stat-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "outside\n").expect("outside file should be written");
        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));

        let request_path = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let response = app.oneshot(file_stat_request(&request_path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_file(outside);
    }

    #[tokio::test]
    async fn file_stat_rejects_remote_panes() {
        let root = unique_temp_dir("file-stat-remote");
        std::fs::write(root.join("main.rs"), "fn main() {}\n").expect("file should be written");

        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, true, None));
        let response = app.oneshot(file_stat_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, Some("dev.ts")));
        let response = app.oneshot(file_stat_request("main.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn file_stat_rejects_missing_and_wrong_auth() {
        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(file_stat_request_with_auth("main.rs", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(file_stat_request_with_auth("main.rs", Some("wrong")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn file_stat_reports_missing_inside_cwd_without_parent_oracle() {
        let root = unique_temp_dir("file-stat-missing");
        let outside_existing_parent = root
            .parent()
            .expect("temp root should have parent")
            .join(format!("taarof-stat-outside-parent-{}", std::process::id()));
        std::fs::create_dir_all(&outside_existing_parent)
            .expect("outside parent should be created");
        let outside_missing_parent = root
            .parent()
            .expect("temp root should have parent")
            .join(format!("taarof-stat-no-parent-{}", std::process::id()));

        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));
        let response = app.oneshot(file_stat_request("missing.rs")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("file stat response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("file stat response should be json");
        assert_eq!(payload["data"]["exists"], false);
        assert_eq!(payload["data"]["is_file"], false);
        assert_eq!(payload["data"]["is_dir"], false);
        assert_eq!(payload["data"]["display_path"], "missing.rs");

        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));
        let request_path = format!(
            "../{}/missing.rs",
            outside_existing_parent
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        let response = app.oneshot(file_stat_request(&request_path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let (app, rx) = test_router("secret");
        spawn_state_responder_with_snapshot(rx, file_preview_state(&root, false, None));
        let request_path = format!(
            "../{}/missing.rs",
            outside_missing_parent
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        let response = app.oneshot(file_stat_request(&request_path)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(outside_existing_parent);
    }

    fn stub_state_snapshot() -> Value {
        serde_json::json!({
            "schema": "taarof.state.v1",
            "session_name": "test",
            "active_workspace": 1,
            "active_tab": 101,
            "dashboard": {},
            "detached_sessions": [],
            "workspaces": [{
                "id": 1,
                "name": "default",
                "tabs": [{
                    "tab_id": 101,
                    "name": "Shell",
                    "panes": [{"pane_id": 1, "shell_running": true}]
                }]
            }]
        })
    }

    fn state_snapshot_projection(state_snapshot: &Value, projection: StateProjection) -> Value {
        match projection {
            StateProjection::Workspaces => state_snapshot["workspaces"].clone(),
            StateProjection::Tabs => {
                let mut tabs = Vec::new();
                if let Some(workspaces) = state_snapshot["workspaces"].as_array() {
                    for workspace in workspaces {
                        let workspace_id = workspace["id"].clone();
                        let workspace_name = workspace["name"].clone();
                        if let Some(workspace_tabs) = workspace["tabs"].as_array() {
                            for tab in workspace_tabs {
                                let mut payload = tab.clone();
                                if let Some(object) = payload.as_object_mut() {
                                    object.insert("workspace_id".into(), workspace_id.clone());
                                    object.insert("workspace_name".into(), workspace_name.clone());
                                }
                                tabs.push(payload);
                            }
                        }
                    }
                }
                Value::Array(tabs)
            }
            StateProjection::Panes => {
                let mut panes = Vec::new();
                if let Some(workspaces) = state_snapshot["workspaces"].as_array() {
                    for workspace in workspaces {
                        let workspace_id = workspace["id"].clone();
                        if let Some(workspace_tabs) = workspace["tabs"].as_array() {
                            for tab in workspace_tabs {
                                let tab_id = tab["tab_id"].clone();
                                if let Some(tab_panes) = tab["panes"].as_array() {
                                    for pane in tab_panes {
                                        let mut payload = pane.clone();
                                        if let Some(object) = payload.as_object_mut() {
                                            object.insert(
                                                "workspace_id".into(),
                                                workspace_id.clone(),
                                            );
                                            object.insert("tab_id".into(), tab_id.clone());
                                        }
                                        panes.push(payload);
                                    }
                                }
                            }
                        }
                    }
                }
                Value::Array(panes)
            }
        }
    }

    fn spawn_state_responder(rx: BridgeReceiver) {
        spawn_state_responder_with_snapshot(rx, stub_state_snapshot());
    }

    fn spawn_state_responder_with_snapshot(rx: BridgeReceiver, state_snapshot: Value) {
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(req) = rx.recv().await {
                match req {
                    HttpBridgeRequest::QueryState { reply } => {
                        let _ = reply.send(state_snapshot.clone());
                    }
                    HttpBridgeRequest::QueryStateProjection { projection, reply } => {
                        let _ = reply.send(state_snapshot_projection(&state_snapshot, projection));
                    }
                    HttpBridgeRequest::QueryHealth { reply } => {
                        let _ = reply.send(serde_json::json!({
                            "state": "ok",
                            "degraded": false,
                            "degraded_components": 0,
                        }));
                    }
                    HttpBridgeRequest::QueryEvents { reply, .. } => {
                        let _ = reply.send(serde_json::json!({"schema": "taarof.events.v1"}));
                    }
                    HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    HttpBridgeRequest::ResolvePaneAttach { reply, .. } => {
                        let _ = reply.send(PaneAttachLookup::NotFound);
                    }
                    HttpBridgeRequest::CaptureVtePaneSnapshot { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::ResolvePtyAdapter { reply, .. } => {
                        let _ = reply.send(PtyAdapterResolution::NotFound);
                    }
                    HttpBridgeRequest::DispatchPtyInput { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::DispatchPtyResize { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                    HttpBridgeRequest::EmitEvent { .. } => {}
                    HttpBridgeRequest::ControlAction { reply, .. } => {
                        let _ = reply.send(Err("not implemented in test bridge".to_string()));
                    }
                }
            }
        });
    }

    fn representative_projection_state() -> AppState {
        let mut state = AppState::new();
        let default_workspace = state.active_workspace;
        seed_headless_terminal_tab(
            &mut state,
            default_workspace,
            "editor",
            HeadlessPaneSeed {
                cwd: Some("/tmp/taarof".into()),
                cwd_host: None,
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ssh_command: None,
                tmux_session: Some("taarof-editor".into()),
                tmux_ssh_target: None,
            },
        )
        .expect("default workspace tab should be seeded");
        seed_headless_terminal_tab(
            &mut state,
            default_workspace,
            "logs",
            HeadlessPaneSeed {
                cwd: Some("/tmp/taarof/logs".into()),
                cwd_host: Some("devbox".into()),
                shell_running: false,
                has_child_process: false,
                remote_shell: true,
                ssh_command: Some(vec!["ssh".into(), "devbox".into()]),
                tmux_session: None,
                tmux_ssh_target: None,
            },
        )
        .expect("second default workspace tab should be seeded");

        let feature_workspace =
            state.create_workspace("feature", Some("/tmp/taarof-feature".into()));
        seed_headless_terminal_tab(
            &mut state,
            feature_workspace,
            "tests",
            HeadlessPaneSeed {
                cwd: Some("/tmp/taarof-feature".into()),
                cwd_host: None,
                shell_running: true,
                has_child_process: false,
                remote_shell: false,
                ssh_command: None,
                tmux_session: Some("taarof-tests".into()),
                tmux_ssh_target: Some("builder".into()),
            },
        )
        .expect("feature workspace tab should be seeded");

        state
    }

    fn projection_and_expected_subset(projection: StateProjection) -> (Value, Value) {
        let state = representative_projection_state();
        let full_snapshot = crate::api::build_state_snapshot(&state);
        let projection_payload = crate::api::build_state_projection(&state, projection);
        let expected_subset = state_snapshot_projection(&full_snapshot, projection);
        assert_eq!(
            projection_payload, expected_subset,
            "projection builder should match full snapshot subset"
        );
        (projection_payload, expected_subset)
    }

    async fn assert_projection_route_matches_full_snapshot_subset(
        path: &str,
        projection: StateProjection,
    ) {
        let (app, rx) = test_router("secret");
        let (projection_payload, expected_subset) = projection_and_expected_subset(projection);
        let bridge_task = tokio::spawn(async move {
            let mut rx = rx;
            match rx.recv().await {
                Some(HttpBridgeRequest::QueryStateProjection {
                    projection: requested,
                    reply,
                }) => {
                    assert_eq!(requested, projection);
                    let _ = reply.send(projection_payload);
                }
                Some(HttpBridgeRequest::QueryState { .. }) => {
                    panic!("narrow projection route must not request full query-state");
                }
                Some(_) => panic!("unexpected bridge request"),
                None => panic!("projection route did not send a bridge request"),
            }
        });

        let response = app
            .oneshot(
                Request::get(path)
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("projection response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("projection response should be json");
        assert_eq!(payload["data"], expected_subset);
        bridge_task
            .await
            .expect("projection bridge task should finish");
    }

    #[tokio::test]
    async fn state_projection_workspaces_route_matches_full_snapshot_subset_without_query_state() {
        assert_projection_route_matches_full_snapshot_subset(
            "/api/v1/workspaces",
            StateProjection::Workspaces,
        )
        .await;
    }

    #[tokio::test]
    async fn state_projection_tabs_route_matches_full_snapshot_subset_without_query_state() {
        assert_projection_route_matches_full_snapshot_subset("/api/v1/tabs", StateProjection::Tabs)
            .await;
    }

    #[tokio::test]
    async fn state_projection_panes_route_matches_full_snapshot_subset_without_query_state() {
        assert_projection_route_matches_full_snapshot_subset(
            "/api/v1/panes",
            StateProjection::Panes,
        )
        .await;
    }

    #[tokio::test]
    async fn sessions_endpoint_returns_session_metadata() {
        let (app, rx) = test_router("secret");
        spawn_state_responder(rx);
        let response = app
            .oneshot(
                Request::get("/api/v1/sessions")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sessions response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("sessions response should be json");
        assert_eq!(payload["data"]["session_name"], serde_json::json!("test"));
    }

    #[tokio::test]
    async fn sessions_endpoint_allows_unnamed_session() {
        let (app, rx) = test_router("secret");
        let mut snapshot = stub_state_snapshot();
        snapshot["session_name"] = Value::Null;
        spawn_state_responder_with_snapshot(rx, snapshot);

        let response = app
            .oneshot(
                Request::get("/api/v1/sessions")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sessions response body should read");
        let payload: Value =
            serde_json::from_slice(&body).expect("sessions response should be json");
        assert_eq!(payload["data"]["session_name"], Value::Null);
    }

    #[tokio::test]
    async fn agent_sessions_endpoint_returns_session_catalog() {
        let (app, rx) = test_router("secret");
        spawn_state_responder(rx);
        let response = app
            .oneshot(
                Request::get("/api/v1/agent-sessions")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn control_request(path: &str, token: Option<&str>, payload: Value) -> Request<Body> {
        let mut builder = Request::post(path).header(CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::from(payload.to_string()))
            .expect("control request should build")
    }

    #[tokio::test]
    async fn http_control_disabled_gate_rejects_authenticated_write() {
        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(control_request(
                "/api/v1/control/run-in-pane",
                Some("secret"),
                serde_json::json!({
                    "pane": 1,
                    "command": "pwd",
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn http_control_split_pane_is_gated_on_control() {
        let (app, _rx) = test_router("secret");
        let response = app
            .oneshot(control_request(
                "/api/v1/control/split-pane",
                Some("secret"),
                serde_json::json!({
                    "direction": "vertical",
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn http_control_enabled_still_requires_bearer_auth() {
        let (app, _rx) = test_router_with_control("secret", true);
        let response = app
            .oneshot(control_request(
                "/api/v1/control/run-in-pane",
                None,
                serde_json::json!({
                    "pane": 1,
                    "command": "pwd",
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_control_split_pane_requires_bearer_auth() {
        let (app, _rx) = test_router_with_control("secret", true);
        let response = app
            .oneshot(control_request(
                "/api/v1/control/split-pane",
                None,
                serde_json::json!({
                    "direction": "vertical",
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn http_control_routes_dispatch_expected_bridge_actions() {
        let (app, mut rx) = test_router_with_control("secret", true);
        let expected_actions = vec![
            HttpControlAction::SendKeys {
                tab: Some("101".to_string()),
                pane: 1,
                keys: "\u{3}".to_string(),
            },
            HttpControlAction::RunInPane {
                tab: None,
                pane: 2,
                command: "pwd".to_string(),
            },
            HttpControlAction::SwitchTab {
                tab: "Shell".to_string(),
            },
            HttpControlAction::CreateTab {
                name: Some("Build".to_string()),
                working_dir: Some("/tmp".to_string()),
                command: Some("echo build".to_string()),
            },
            HttpControlAction::SplitPane {
                tab: Some("101".to_string()),
                direction: Some("horizontal".to_string()),
                command: Some("htop".to_string()),
                working_dir: Some("/srv".to_string()),
            },
        ];

        tokio::spawn(async move {
            for expected in expected_actions {
                match rx.recv().await {
                    Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                        assert_eq!(action, expected);
                        let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                    }
                    Some(_) => panic!("expected HTTP control action bridge request"),
                    None => panic!("bridge request channel closed"),
                }
            }
        });

        let requests = [
            control_request(
                "/api/v1/control/send-keys",
                Some("secret"),
                serde_json::json!({
                    "tab": "101",
                    "pane": 1,
                    "keys": "\u{3}",
                }),
            ),
            control_request(
                "/api/v1/control/run-in-pane",
                Some("secret"),
                serde_json::json!({
                    "pane": 2,
                    "command": "pwd",
                }),
            ),
            control_request(
                "/api/v1/control/switch-tab",
                Some("secret"),
                serde_json::json!({
                    "tab": "Shell",
                }),
            ),
            control_request(
                "/api/v1/control/create-tab",
                Some("secret"),
                serde_json::json!({
                    "name": "Build",
                    "working_dir": "/tmp",
                    "command": "echo build",
                }),
            ),
            control_request(
                "/api/v1/control/split-pane",
                Some("secret"),
                serde_json::json!({
                    "tab": "101",
                    "direction": "horizontal",
                    "command": "htop",
                    "working_dir": "/srv",
                }),
            ),
        ];

        for request in requests {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("control response body should read");
            let payload: Value =
                serde_json::from_slice(&body).expect("control response should be json");
            assert_eq!(payload["ok"], serde_json::json!(true));
            assert_eq!(payload["data"]["accepted"], serde_json::json!(true));
        }
    }

    #[tokio::test]
    async fn http_tmux_control_waits_past_the_former_five_second_bridge_limit() {
        let (app, mut rx) = test_router_with_control("secret", true);

        tokio::spawn(async move {
            match rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert!(matches!(action, HttpControlAction::SendKeys { .. }));
                    tokio::time::sleep(Duration::from_millis(5_100)).await;
                    let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                }
                Some(_) => panic!("expected HTTP control action bridge request"),
                None => panic!("bridge request channel closed"),
            }
        });

        let response = app
            .oneshot(control_request(
                "/api/v1/control/send-keys",
                Some("secret"),
                serde_json::json!({
                    "pane": 1,
                    "keys": "echo still-honest",
                }),
            ))
            .await
            .expect("control response should complete");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_control_actual_timeout_cancels_late_apply() {
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let pane_snapshotter = default_pane_snapshotter(bridge_tx.clone());
        let state = test_http_state(
            "secret",
            bridge_tx,
            pane_snapshotter,
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        let mutated = Arc::new(AtomicBool::new(false));
        let mutated_for_responder = Arc::clone(&mutated);

        tokio::spawn(async move {
            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { guard, reply, .. }) => {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    if guard.try_begin_apply() {
                        mutated_for_responder.store(true, Ordering::Release);
                        guard.finish();
                        let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                    }
                }
                Some(_) => panic!("expected HTTP control action bridge request"),
                None => panic!("bridge request channel closed"),
            }
        });

        let result = send_control_action_value_with_timeout(
            &state,
            HttpControlAction::SendKeys {
                tab: None,
                pane: 1,
                keys: "echo too-late".to_string(),
            },
            Duration::from_millis(20),
        )
        .await;
        let error = match result {
            Ok(value) => panic!("timed-out control unexpectedly succeeded: {value}"),
            Err(error) => error,
        };
        assert_eq!(error.into_response().status(), StatusCode::GATEWAY_TIMEOUT);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !mutated.load(Ordering::Acquire),
            "cancelled control must not apply after its 504"
        );
    }

    #[tokio::test]
    async fn http_control_does_not_timeout_after_mutation_claims_apply() {
        let (bridge_tx, mut bridge_rx) = mpsc::channel(8);
        let pane_snapshotter = default_pane_snapshotter(bridge_tx.clone());
        let state = test_http_state(
            "secret",
            bridge_tx,
            pane_snapshotter,
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );

        tokio::spawn(async move {
            match bridge_rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { guard, reply, .. }) => {
                    assert!(guard.try_begin_apply(), "mutation should claim apply");
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    guard.finish();
                    let _ = reply.send(Ok(serde_json::json!({"accepted": true})));
                }
                Some(_) => panic!("expected HTTP control action bridge request"),
                None => panic!("bridge request channel closed"),
            }
        });

        let result = send_control_action_value_with_timeout(
            &state,
            HttpControlAction::ResizePane {
                tab: None,
                pane: 1,
                cols: 120,
                rows: 40,
            },
            Duration::from_millis(20),
        )
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => panic!("claimed mutation must return its exact result instead of a 504"),
        };
        assert_eq!(result["accepted"], true);
    }

    #[tokio::test]
    async fn http_control_bridge_errors_return_message_body() {
        let (app, mut rx) = test_router_with_control("secret", true);

        tokio::spawn(async move {
            match rx.recv().await {
                Some(HttpBridgeRequest::ControlAction { action, reply, .. }) => {
                    assert_eq!(
                        action,
                        HttpControlAction::RunInPane {
                            tab: None,
                            pane: 999,
                            command: "pwd".to_string(),
                        }
                    );
                    let _ = reply.send(Err("pane not found".to_string()));
                }
                Some(_) => panic!("expected HTTP control action bridge request"),
                None => panic!("bridge request channel closed"),
            }
        });

        let response = app
            .oneshot(control_request(
                "/api/v1/control/run-in-pane",
                Some("secret"),
                serde_json::json!({
                    "pane": 999,
                    "command": "pwd",
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("control response body should read");
        let payload: Value = serde_json::from_slice(&body).expect("control error should be json");
        assert_eq!(payload["ok"], serde_json::json!(false));
        assert_eq!(payload["error"], serde_json::json!("pane not found"));
    }

    #[test]
    fn http_control_cannot_enable_on_non_loopback_bind() {
        let config = crate::config::HttpControlConfig { enabled: true };

        assert!(http_control_enabled_for_bind(
            &config,
            SocketAddr::from(([127, 0, 0, 1], 7800))
        ));
        assert!(!http_control_enabled_for_bind(
            &config,
            SocketAddr::from(([0, 0, 0, 0], 7800))
        ));
    }

    #[test]
    fn check_auth_accepts_valid_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret123".parse().unwrap());
        assert!(check_auth(&headers, "secret123").is_ok());
    }

    #[test]
    fn check_auth_rejects_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_auth(&headers, "secret123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn check_auth_rejects_wrong_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert_eq!(
            check_auth(&headers, "secret123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn check_auth_rejects_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic secret123".parse().unwrap());
        assert_eq!(
            check_auth(&headers, "secret123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn check_ws_auth_accepts_query_token() {
        let headers = HeaderMap::new();
        assert!(check_ws_auth(&headers, Some("secret123"), "secret123").is_ok());
    }

    #[test]
    fn check_ws_auth_prefers_header_or_query_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret123".parse().unwrap());
        assert!(check_ws_auth(&headers, Some("secret123"), "secret123").is_ok());
    }

    #[test]
    fn check_ws_auth_accepts_valid_query_token_when_header_is_wrong() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer stale".parse().unwrap());
        assert!(check_ws_auth(&headers, Some("secret123"), "secret123").is_ok());
    }

    #[test]
    fn check_ws_auth_rejects_missing_both_auth_channels() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_ws_auth(&headers, None, "secret123"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn generate_token_is_nonempty_hex() {
        let token = generate_token().expect("token generation should succeed");
        assert_eq!(token.len(), HTTP_TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn write_token_file_reports_parent_directory_errors() {
        let token_path = unique_temp_dir("missing-token-parent")
            .join("missing")
            .join("taarof.token");
        let error = write_token_file(&token_path, "secret")
            .expect_err("missing parent directory should fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn sanitize_web_path_rejects_parent_segments() {
        assert!(sanitize_web_path("../dist/index.html").is_none());
        assert!(sanitize_web_path("assets/../index.html").is_none());
    }

    #[test]
    fn source_checkout_runs_prefer_repo_bundle_before_xdg_data() {
        let temp_root = unique_temp_dir("source-precedence");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");
        let source_exe = repo_root.join("taarof-app/target/debug/taarof-app");
        let data_dir = temp_root.join("xdg-data");
        let xdg_bundle = data_dir.join("taarof/web");

        std::fs::create_dir_all(&repo_dist).expect("repo dist should exist");
        std::fs::create_dir_all(&xdg_bundle).expect("xdg bundle should exist");
        std::fs::create_dir_all(source_exe.parent().expect("exe parent should exist"))
            .expect("source exe parent should exist");
        std::fs::write(&source_exe, "").expect("source exe placeholder should exist");
        std::fs::write(repo_dist.join("index.html"), "repo bundle")
            .expect("repo index should be written");
        std::fs::write(xdg_bundle.join("index.html"), "xdg bundle")
            .expect("xdg index should be written");

        let candidates =
            web_asset_candidate_paths_for(None, Some(&source_exe), Some(data_dir), &repo_root);
        assert_eq!(candidates[0], repo_dist);
        assert_eq!(candidates[1], xdg_bundle);

        let resolved = resolve_web_assets(&candidates).expect("repo bundle should resolve");
        assert_eq!(resolved.dist_dir, repo_root.join("taarof-web/dist"));
    }

    #[test]
    fn missing_env_override_falls_back_to_repo_bundle_when_running_from_source() {
        let temp_root = unique_temp_dir("env-override");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");
        let source_exe = repo_root.join("taarof-app/target/debug/taarof-app");
        let data_dir = temp_root.join("xdg-data");
        let xdg_bundle = data_dir.join("taarof/web");
        let missing_override = temp_root.join("custom-web-dist");

        std::fs::create_dir_all(&repo_dist).expect("repo dist should exist");
        std::fs::create_dir_all(&xdg_bundle).expect("xdg bundle should exist");
        std::fs::create_dir_all(source_exe.parent().expect("exe parent should exist"))
            .expect("source exe parent should exist");
        std::fs::write(&source_exe, "").expect("source exe placeholder should exist");
        std::fs::write(repo_dist.join("index.html"), "repo bundle")
            .expect("repo index should be written");
        std::fs::write(xdg_bundle.join("index.html"), "xdg bundle")
            .expect("xdg index should be written");

        let candidates = web_asset_candidate_paths_for(
            Some(missing_override.clone()),
            Some(&source_exe),
            Some(data_dir),
            &repo_root,
        );
        assert_eq!(candidates[0], missing_override);
        assert_eq!(candidates[1], repo_dist);
        assert_eq!(candidates[2], xdg_bundle);

        let resolved = resolve_web_assets(&candidates).expect("repo bundle should resolve");
        assert_eq!(resolved.dist_dir, repo_root.join("taarof-web/dist"));
    }

    #[test]
    fn source_checkout_root_is_recovered_from_exe_path_without_recorded_provenance() {
        let temp_root = unique_temp_dir("exe-relative-source-root");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");
        let test_exe = repo_root.join("taarof-app/target/debug/deps/runtime_smoke-abc123");
        let installed_exe = temp_root.join("prefix/bin/taarof-app");

        std::fs::create_dir_all(&repo_dist).expect("repo dist should exist");
        std::fs::create_dir_all(test_exe.parent().expect("exe parent should exist"))
            .expect("test exe parent should exist");
        std::fs::create_dir_all(installed_exe.parent().expect("exe parent should exist"))
            .expect("installed exe parent should exist");
        std::fs::write(repo_root.join("taarof-app/Cargo.toml"), "[package]\n")
            .expect("manifest placeholder should be written");
        std::fs::write(&test_exe, "").expect("test exe placeholder should exist");
        std::fs::write(&installed_exe, "").expect("installed exe placeholder should exist");

        // A cargo-built binary lives under <repo>/taarof-app/target/..., so the
        // checkout is recoverable without embedding the builder's path.
        assert_eq!(
            source_checkout_root_from_exe_path(&test_exe),
            Some(repo_root.clone())
        );
        // An installed binary must not claim an unrelated directory as a checkout.
        assert_eq!(source_checkout_root_from_exe_path(&installed_exe), None);
    }

    #[test]
    fn missing_env_override_falls_back_to_installed_bundle() {
        let temp_root = unique_temp_dir("env-override-installed");
        let prefix = temp_root.join("prefix");
        let installed_exe = prefix.join("bin/taarof-app");
        let installed_bundle = prefix.join("share/taarof/web");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");
        let missing_override = temp_root.join("custom-web-dist");

        std::fs::create_dir_all(installed_exe.parent().expect("exe parent should exist"))
            .expect("installed exe parent should exist");
        std::fs::create_dir_all(&installed_bundle).expect("installed bundle should exist");
        std::fs::create_dir_all(&repo_dist).expect("repo dist should exist");
        std::fs::write(&installed_exe, "").expect("installed exe placeholder should exist");
        std::fs::write(installed_bundle.join("index.html"), "installed bundle")
            .expect("installed index should be written");
        std::fs::write(repo_dist.join("index.html"), "repo bundle")
            .expect("repo index should be written");

        let candidates = web_asset_candidate_paths_for(
            Some(missing_override.clone()),
            Some(&installed_exe),
            None,
            &repo_root,
        );
        assert_eq!(candidates[0], missing_override);
        assert_eq!(candidates[1], installed_bundle);
        assert_eq!(candidates[2], repo_dist);

        let resolved = resolve_web_assets(&candidates)
            .expect("installed bundle should resolve after override");
        assert_eq!(resolved.dist_dir, prefix.join("share/taarof/web"));
    }

    #[test]
    fn installed_binary_runs_prefer_exe_relative_bundle_before_repo_bundle() {
        let temp_root = unique_temp_dir("installed-precedence");
        let prefix = temp_root.join("prefix");
        let installed_exe = prefix.join("bin/taarof-app");
        let installed_bundle = prefix.join("share/taarof/web");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");

        std::fs::create_dir_all(installed_exe.parent().expect("exe parent should exist"))
            .expect("installed exe parent should exist");
        std::fs::create_dir_all(&installed_bundle).expect("installed bundle should exist");
        std::fs::create_dir_all(&repo_dist).expect("repo dist should exist");
        std::fs::write(&installed_exe, "").expect("installed exe placeholder should exist");
        std::fs::write(installed_bundle.join("index.html"), "installed bundle")
            .expect("installed index should be written");
        std::fs::write(repo_dist.join("index.html"), "repo bundle")
            .expect("repo index should be written");

        let candidates =
            web_asset_candidate_paths_for(None, Some(&installed_exe), None, &repo_root);
        assert_eq!(candidates[0], installed_bundle);
        assert_eq!(candidates[1], repo_dist);

        let resolved =
            resolve_web_assets(&candidates).expect("installed bundle should resolve first");
        assert_eq!(resolved.dist_dir, prefix.join("share/taarof/web"));
    }

    #[test]
    fn installed_web_dist_dir_from_exe_path_resolves_prefix_share_dir() {
        let exe_path = Path::new("/tmp/taarof-prefix/bin/taarof-app");
        assert_eq!(
            installed_web_dist_dir_from_exe_path(exe_path),
            Some(PathBuf::from("/tmp/taarof-prefix/share/taarof/web"))
        );
    }

    #[tokio::test]
    async fn serve_web_request_prefers_installed_bundle_for_installed_layout() {
        let temp_root = unique_temp_dir("serve-installed-layout");
        let prefix = temp_root.join("prefix");
        let installed_exe = prefix.join("bin/taarof-app");
        let installed_bundle = prefix.join("share/taarof/web");
        let repo_root = temp_root.join("repo");
        let repo_dist = repo_root.join("taarof-web/dist");

        std::fs::create_dir_all(installed_exe.parent().expect("exe parent should exist"))
            .expect("installed exe parent should exist");
        std::fs::create_dir_all(installed_bundle.join("assets"))
            .expect("installed assets dir should exist");
        std::fs::create_dir_all(repo_dist.join("assets")).expect("repo assets dir should exist");
        std::fs::write(&installed_exe, "").expect("installed exe placeholder should exist");
        std::fs::write(
            installed_bundle.join("index.html"),
            "<!doctype html><html><body>installed bundle</body></html>",
        )
        .expect("installed index should be written");
        std::fs::write(
            installed_bundle.join("assets/app.js"),
            "console.log('installed bundle');",
        )
        .expect("installed asset should be written");
        std::fs::write(
            repo_dist.join("index.html"),
            "<!doctype html><html><body>repo bundle</body></html>",
        )
        .expect("repo index should be written");
        std::fs::write(
            repo_dist.join("assets/app.js"),
            "console.log('repo bundle');",
        )
        .expect("repo asset should be written");

        let candidates =
            web_asset_candidate_paths_for(None, Some(&installed_exe), None, &repo_root);
        assert_eq!(candidates[0], installed_bundle);
        assert_eq!(candidates[1], repo_dist);

        let (bridge_tx, _bridge_rx) = mpsc::channel(8);
        let (event_tx, _) = broadcast::channel(16);
        let pane_snapshotter = default_pane_snapshotter(bridge_tx.clone());
        let pane_snapshot_slots = Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS));
        let pane_dirty = Arc::new(PaneDirtyRegistry::default());
        let pane_attach_hub = PaneAttachHub::new(
            bridge_tx.clone(),
            Arc::clone(&pane_snapshotter),
            Arc::clone(&pane_snapshot_slots),
            Arc::clone(&pane_dirty),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        let state = HttpState {
            bridge: bridge_tx.clone(),
            auth_token: Arc::new("secret".to_string()),
            control_enabled: false,
            bind_is_loopback: true,
            runtime_id: Arc::new(generate_runtime_id()),
            agent_session_catalog: Arc::new(
                crate::agent_sessions::AgentSessionCatalog::with_scanner(Arc::new(
                    FixedAgentSessionScanner::empty(),
                )),
            ),
            event_broadcast: event_tx,
            web_asset_candidates: Arc::new(candidates),
            pane_snapshotter,
            pane_attach_slots: Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_CONNECTIONS)),
            pane_snapshot_slots,
            pane_attach_hub,
            history_reader: crate::history::HistoryReader::disabled(),
        };

        let root = serve_web_request(&state, "/");
        assert_eq!(root.status(), StatusCode::OK);
        let root_body = axum::body::to_bytes(root.into_body(), usize::MAX)
            .await
            .expect("root body should be readable");
        let root_body = std::str::from_utf8(&root_body).expect("root body should be utf-8");
        assert!(root_body.contains("installed bundle"));
        assert!(!root_body.contains("repo bundle"));

        let asset = serve_web_request(&state, "/assets/app.js");
        assert_eq!(asset.status(), StatusCode::OK);
        let asset_body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .expect("asset body should be readable");
        let asset_body = std::str::from_utf8(&asset_body).expect("asset body should be utf-8");
        assert!(asset_body.contains("installed bundle"));
        assert!(!asset_body.contains("repo bundle"));
    }

    #[tokio::test]
    async fn serve_web_request_rejects_symlink_escape_but_serves_normal_assets() {
        // EXAMPLE-45: sanitize_web_path blocks `..`, but std::fs::read follows
        // symlinks. A symlink placed inside the dist dir pointing OUTSIDE it must
        // not be served (path-confinement bypass), while real in-dir assets keep
        // serving normally.
        use std::os::unix::fs::symlink;

        let temp_root = unique_temp_dir("symlink-escape");
        let dist_dir = temp_root.join("dist");
        let outside_dir = temp_root.join("outside");
        std::fs::create_dir_all(&dist_dir).expect("dist dir should exist");
        std::fs::create_dir_all(&outside_dir).expect("outside dir should exist");

        // Normal in-dir asset that must continue to serve.
        std::fs::write(dist_dir.join("index.html"), "spa index").expect("index should be written");
        std::fs::write(dist_dir.join("app.js"), "console.log('in-dist asset');")
            .expect("normal asset should be written");

        // Secret file OUTSIDE the dist dir, exposed via a symlink placed INSIDE it.
        let secret_path = outside_dir.join("secret.txt");
        std::fs::write(&secret_path, "TOP SECRET OUTSIDE DIST")
            .expect("secret file should be written");
        symlink(&secret_path, dist_dir.join("escape.txt"))
            .expect("escape symlink should be created");

        let state = test_http_state_with_web_assets(vec![dist_dir.clone()]);

        // The symlink-escape request must NOT serve the outside target.
        let escape = serve_web_request(&state, "/escape.txt");
        assert_eq!(escape.status(), StatusCode::NOT_FOUND);
        let escape_body = axum::body::to_bytes(escape.into_body(), usize::MAX)
            .await
            .expect("escape body should be readable");
        let escape_body = std::str::from_utf8(&escape_body).expect("escape body should be utf-8");
        assert!(
            !escape_body.contains("TOP SECRET"),
            "symlink target outside dist dir must not be served"
        );
        assert_eq!(escape_body, "asset not found");

        // A normal asset under dist_dir must still serve correctly.
        let asset = serve_web_request(&state, "/app.js");
        assert_eq!(asset.status(), StatusCode::OK);
        let asset_body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .expect("asset body should be readable");
        let asset_body = std::str::from_utf8(&asset_body).expect("asset body should be utf-8");
        assert!(asset_body.contains("in-dist asset"));
    }

    #[tokio::test]
    async fn bare_api_path_returns_not_found_instead_of_spa() {
        let temp_root = unique_temp_dir("api-fallback");
        let dist_dir = temp_root.join("dist");
        std::fs::create_dir_all(&dist_dir).expect("dist dir should exist");
        std::fs::write(dist_dir.join("index.html"), "spa index").expect("index should be written");

        let (bridge_tx, _bridge_rx) = mpsc::channel(8);
        let (event_tx, _) = broadcast::channel(16);
        let pane_snapshotter = default_pane_snapshotter(bridge_tx.clone());
        let pane_snapshot_slots = Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_SNAPSHOT_WORKERS));
        let pane_dirty = Arc::new(PaneDirtyRegistry::default());
        let pane_attach_hub = PaneAttachHub::new(
            bridge_tx.clone(),
            Arc::clone(&pane_snapshotter),
            Arc::clone(&pane_snapshot_slots),
            Arc::clone(&pane_dirty),
            HTTP_PANE_ATTACH_POLL_INTERVAL,
        );
        let state = HttpState {
            bridge: bridge_tx.clone(),
            auth_token: Arc::new("secret".to_string()),
            control_enabled: false,
            bind_is_loopback: true,
            runtime_id: Arc::new(generate_runtime_id()),
            agent_session_catalog: Arc::new(
                crate::agent_sessions::AgentSessionCatalog::with_scanner(Arc::new(
                    FixedAgentSessionScanner::empty(),
                )),
            ),
            event_broadcast: event_tx,
            web_asset_candidates: Arc::new(vec![dist_dir]),
            pane_snapshotter,
            pane_attach_slots: Arc::new(Semaphore::new(HTTP_PANE_ATTACH_MAX_CONNECTIONS)),
            pane_snapshot_slots,
            pane_attach_hub,
            history_reader: crate::history::HistoryReader::disabled(),
        };

        let response = serve_web_request(&state, "/api");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        assert_eq!(std::str::from_utf8(&body).unwrap(), "not found");
    }

    #[tokio::test]
    async fn serve_web_request_returns_generic_503_when_no_web_bundle_exists() {
        let missing_dir = unique_temp_dir("missing-web-bundle").join("missing");
        let state = test_http_state_with_web_assets(vec![missing_dir.clone()]);

        let response = serve_web_request(&state, "/");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        let body = std::str::from_utf8(&body).expect("response body should be utf-8");
        assert_eq!(body, MISSING_WEB_DIST_RESPONSE_BODY);
        assert!(body.contains("web bundle missing"));
        assert!(!body.contains("npm run build"));
        assert!(!body.contains(&missing_dir.display().to_string()));
        assert!(!body.contains(MISSING_WEB_DIST_DIAGNOSTIC_PREFIX));
    }

    #[tokio::test]
    async fn serve_web_request_returns_generic_503_for_nested_paths_when_no_web_bundle_exists() {
        let missing_dir = unique_temp_dir("missing-web-bundle").join("missing");
        let state = test_http_state_with_web_assets(vec![missing_dir.clone()]);

        let response = serve_web_request(&state, "/assets/app.js");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        let body = std::str::from_utf8(&body).expect("response body should be utf-8");
        assert_eq!(body, MISSING_WEB_DIST_RESPONSE_BODY);
        assert!(!body.contains("assets/app.js"));
        assert!(!body.contains(&missing_dir.display().to_string()));
        assert!(!body.contains(MISSING_WEB_DIST_DIAGNOSTIC_PREFIX));
    }

    #[test]
    fn missing_web_dist_diagnostic_mentions_attempted_paths() {
        let missing_dir = PathBuf::from("/tmp/taarof-web-dist");
        let missing_dir_text = missing_dir.display().to_string();
        let diagnostic = missing_web_dist_diagnostic(std::slice::from_ref(&missing_dir));

        assert!(diagnostic.contains(MISSING_WEB_DIST_DIAGNOSTIC_PREFIX));
        assert!(diagnostic.contains(&missing_dir_text));
    }

    #[tokio::test]
    async fn missing_web_dist_response_body_is_pathless_constant() {
        // KNOWN_ISSUES.md #20: the 503 fallback must never echo the
        // attempted asset paths into the response body. The body is the
        // pathless constant; paths belong only in the diagnostic log line.
        let attempted_paths = vec![
            PathBuf::from("/tmp/taarof-web-dist/primary"),
            PathBuf::from("/opt/taarof/share/web/secondary"),
            PathBuf::from("C:\\taarof\\web\\tertiary"),
        ];

        let response = missing_web_dist_response(&attempted_paths);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        let body = std::str::from_utf8(&body).expect("response body should be utf-8");

        assert_eq!(body, MISSING_WEB_DIST_RESPONSE_BODY);
        assert!(!body.contains(MISSING_WEB_DIST_DIAGNOSTIC_PREFIX));
        for attempted in &attempted_paths {
            let attempted_text = attempted.display().to_string();
            assert!(
                !body.contains(&attempted_text),
                "missing-web-dist 503 body leaked attempted path {attempted_text:?}",
            );
        }
        // The body must not include path-shaped strings either.
        for forbidden in ["/tmp/taarof-web-dist", "taarof/share/web", "taarof\\web"] {
            assert!(
                !body.contains(forbidden),
                "missing-web-dist 503 body leaked forbidden substring {forbidden:?}",
            );
        }
    }

    #[tokio::test]
    async fn missing_web_dist_response_body_is_pathless_with_empty_attempted_paths() {
        // KNOWN_ISSUES.md #20: even with an empty attempted-paths slice, the
        // body must still be the pathless constant (no fallback echo, no
        // empty-list placeholder, no diagnostic prefix).
        let response = missing_web_dist_response(&[]);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        let body = std::str::from_utf8(&body).expect("response body should be utf-8");

        assert_eq!(body, MISSING_WEB_DIST_RESPONSE_BODY);
        assert!(!body.contains(MISSING_WEB_DIST_DIAGNOSTIC_PREFIX));
        assert!(!body.contains("attempted"));
    }

    #[test]
    fn resolve_pane_attach_target_marks_headless_panes_with_tmux_attachable() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "tmux tab",
            HeadlessPaneSeed {
                cwd: Some("/tmp/project".into()),
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                tmux_session: Some("taarof-session".into()),
                ..HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");

        assert_eq!(
            resolve_pane_attach_target(&state, pane_id),
            PaneAttachLookup::Attachable(PaneAttachTarget {
                tab_id,
                pane_id,
                kind: PaneAttachKind::Tmux {
                    session_name: "taarof-session".into(),
                    target: crate::tmux::TmuxTarget::Local,
                },
            })
        );
    }

    #[test]
    fn resolve_pane_attach_target_marks_headless_panes_without_tmux_unsupported() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (_tab_id, pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "plain tab",
            HeadlessPaneSeed {
                cwd: Some("/tmp/project".into()),
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ..HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");

        assert_eq!(
            resolve_pane_attach_target(&state, pane_id),
            PaneAttachLookup::Unsupported
        );
    }

    #[test]
    fn resolve_pane_attach_target_in_tab_disambiguates_duplicate_pane_ids() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (first_tab_id, shared_pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "first tab",
            HeadlessPaneSeed {
                shell_running: true,
                tmux_session: Some("first-session".into()),
                ..HeadlessPaneSeed::default()
            },
        )
        .expect("first headless pane should be seeded");
        let (second_tab_id, original_second_pane_id) = seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "second tab",
            HeadlessPaneSeed {
                shell_running: true,
                tmux_session: Some("second-session".into()),
                ..HeadlessPaneSeed::default()
            },
        )
        .expect("second headless pane should be seeded");

        let second_state = state
            .headless_panes
            .remove(&(second_tab_id, original_second_pane_id))
            .expect("second headless pane state should exist");
        state
            .headless_panes
            .insert((second_tab_id, shared_pane_id), second_state);
        let second_tab = state
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == second_tab_id)
            .expect("second tab should exist");
        *second_tab.panes = crate::pane::PaneNode::Stub {
            pane_id: shared_pane_id,
        };
        second_tab.focused_pane_id = shared_pane_id;

        assert_eq!(
            resolve_pane_attach_target(&state, shared_pane_id),
            PaneAttachLookup::Attachable(PaneAttachTarget {
                tab_id: first_tab_id,
                pane_id: shared_pane_id,
                kind: PaneAttachKind::Tmux {
                    session_name: "first-session".into(),
                    target: crate::tmux::TmuxTarget::Local,
                },
            })
        );
        assert_eq!(
            resolve_pane_attach_target_in_tab(&state, second_tab_id, shared_pane_id),
            PaneAttachLookup::Attachable(PaneAttachTarget {
                tab_id: second_tab_id,
                pane_id: shared_pane_id,
                kind: PaneAttachKind::Tmux {
                    session_name: "second-session".into(),
                    target: crate::tmux::TmuxTarget::Local,
                },
            })
        );
    }

    #[test]
    fn pane_snapshotter_dispatches_vte_kind_to_vte_path() {
        // Verify a Vte target takes the bridge arm by sending into a closed channel
        // and observing the bridge-closed error. The receiver is dropped before the
        // call so `bridge.send().await` returns Err immediately and the call does
        // not hang waiting on the never-replied oneshot.
        let (tx, rx) = mpsc::channel::<HttpBridgeRequest>(1);
        drop(rx);
        let snapshotter = default_pane_snapshotter(tx);
        let target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Vte,
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(snapshotter(target, false)).unwrap_err();
        assert!(
            err.contains("bridge"),
            "expected vte path to hit bridge send, got: {err}"
        );
    }

    #[test]
    fn snapshot_frame_json_base64_roundtrips_utf8_payload() {
        let target = PaneAttachTarget {
            tab_id: 42,
            pane_id: 7,
            kind: PaneAttachKind::Vte,
        };
        let snapshot = PaneSnapshot {
            output: "Vault CLI loaded. ☕\nworkstation:sample-project main ? }".into(),
            width: 120,
            height: 40,
        };

        let frame = snapshot_frame_json(&target, &snapshot);
        assert_eq!(frame["type"], serde_json::json!("snapshot"));
        assert_eq!(frame["encoding"], serde_json::json!("base64"));
        assert_eq!(frame["cols"], serde_json::json!(120));
        assert_eq!(frame["rows"], serde_json::json!(40));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(frame["payload"].as_str().expect("payload should be string"))
            .expect("snapshot payload should be base64");
        assert_eq!(decoded, snapshot.output.as_bytes());
    }

    #[test]
    fn replace_frame_json_keeps_utf8_payload_for_xterm_replace() {
        let target = PaneAttachTarget {
            tab_id: 42,
            pane_id: 7,
            kind: PaneAttachKind::Vte,
        };
        let snapshot = PaneSnapshot {
            output: "new prompt\n".into(),
            width: 80,
            height: 24,
        };

        let frame = replace_frame_json(&target, &snapshot);
        assert_eq!(frame["type"], serde_json::json!("replace"));
        assert_eq!(frame["encoding"], serde_json::json!("utf8"));
        assert_eq!(frame["payload"], serde_json::json!(snapshot.output));
    }

    #[test]
    fn control_terminal_update_frame_json_uses_delta_only_for_append_same_size() {
        let target = PaneAttachTarget {
            tab_id: 42,
            pane_id: 7,
            kind: PaneAttachKind::Vte,
        };
        let appended = PaneSnapshot {
            output: "ready\nnext".into(),
            width: 80,
            height: 24,
        };

        let delta = control_terminal_update_frame_json(&target, &appended, "ready", (80, 24))
            .expect("append should produce a frame");
        assert_eq!(delta["type"], serde_json::json!("delta"));
        assert_eq!(delta["encoding"], serde_json::json!("utf8"));
        assert_eq!(delta["payload"], serde_json::json!("\nnext"));

        let unchanged =
            control_terminal_update_frame_json(&target, &appended, "ready\nnext", (80, 24));
        assert!(
            unchanged.is_none(),
            "unchanged output should not emit an empty delta"
        );

        let rewritten = PaneSnapshot {
            output: "fresh".into(),
            width: 80,
            height: 24,
        };
        let replace = control_terminal_update_frame_json(&target, &rewritten, "ready", (80, 24))
            .expect("rewrite should produce a replace frame");
        assert_eq!(replace["type"], serde_json::json!("replace"));
        assert_eq!(replace["payload"], serde_json::json!("fresh"));

        let resized = control_terminal_update_frame_json(&target, &appended, "ready", (100, 30))
            .expect("size changes should produce a replace frame");
        assert_eq!(resized["type"], serde_json::json!("replace"));
    }

    #[test]
    fn pane_attach_baseline_skips_first_replace_when_plain_text_matches_snapshot() {
        let target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let snapshot = PaneSnapshot {
            output: "ready".into(),
            width: 80,
            height: 24,
        };
        let replace = PaneSnapshot {
            output: "ready".into(),
            width: 80,
            height: 24,
        };

        let baseline = PaneAttachBaseline::from_replace_snapshot(&target, &snapshot);
        assert!(!baseline.has_changed(&target, &replace));
    }

    #[test]
    fn pane_attach_baseline_forces_refresh_when_target_changes() {
        let original_target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session-a".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let replacement_target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session-b".into(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".into(),
                },
            },
        };
        let snapshot = PaneSnapshot {
            output: "ready".into(),
            width: 80,
            height: 24,
        };

        let baseline = PaneAttachBaseline {
            target: Some(original_target),
            replace_payload: Some("ready".into()),
            size: Some((80, 24)),
        };
        assert!(baseline.has_changed(&replacement_target, &snapshot));
    }

    #[tokio::test]
    async fn tab_scoped_vte_attach_route_serializes_snapshot_for_xterm() {
        let target = PaneAttachTarget {
            tab_id: 42,
            pane_id: 7,
            kind: PaneAttachKind::Vte,
        };
        let (bridge_tx, bridge_rx) = mpsc::channel(8);
        let state = test_http_state(
            "secret",
            bridge_tx,
            scripted_snapshotter(vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "Vault CLI loaded. ☕\nworkstation:sample-project main ? }".into(),
                        width: 120,
                        height: 40,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "Vault CLI loaded. ☕\nworkstation:sample-project main ? }".into(),
                        width: 120,
                        height: 40,
                    },
                },
            ]),
            Duration::from_millis(20),
        );
        spawn_pane_attach_responder(
            bridge_rx,
            vec![PaneAttachLookup::Attachable(target.clone())],
            PaneAttachLookup::Attachable(target),
        );

        let (addr, server) = spawn_ws_test_server(state).await;
        let url = format!("ws://{addr}/api/v1/tabs/42/panes/7/attach?token=secret");
        let (mut socket, response) = connect_async(url)
            .await
            .expect("websocket client should connect");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        let first_frame = socket
            .next()
            .await
            .expect("snapshot frame should arrive")
            .expect("snapshot frame should be ok")
            .into_text()
            .expect("snapshot frame should be text");
        let first_frame: Value =
            serde_json::from_str(&first_frame).expect("snapshot frame should be valid json");
        assert_eq!(first_frame["type"], serde_json::json!("snapshot"));
        assert_eq!(first_frame["encoding"], serde_json::json!("base64"));
        assert_eq!(first_frame["cols"], serde_json::json!(120));
        assert_eq!(first_frame["rows"], serde_json::json!(40));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(
                first_frame["payload"]
                    .as_str()
                    .expect("payload should be a string"),
            )
            .expect("snapshot payload should decode");
        assert_eq!(
            String::from_utf8(decoded).expect("snapshot payload should be utf8"),
            "Vault CLI loaded. ☕\nworkstation:sample-project main ? }"
        );

        let _ = socket.close(None).await;
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn pane_attach_route_keeps_initial_snapshot_without_idle_first_replace() {
        let target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let fixture = PaneAttachFixture::new(
            "secret",
            vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "\u{1b}(0qqq\u{1b}(B".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "\u{2500}\u{2500}\u{2500}".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "\u{2500}\u{2500}\u{2500}".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ],
            vec![
                PaneAttachLookup::Attachable(target.clone()),
                PaneAttachLookup::Attachable(target.clone()),
            ],
            PaneAttachLookup::Attachable(target),
        )
        .await;

        let mut socket = fixture.connect_pane(7, "secret").await;

        let first_frame = socket
            .next()
            .await
            .expect("snapshot frame should arrive")
            .expect("snapshot frame should be ok")
            .into_text()
            .expect("snapshot frame should be text");
        let first_frame: Value =
            serde_json::from_str(&first_frame).expect("snapshot frame should be valid json");
        assert_eq!(first_frame["type"], serde_json::json!("snapshot"));
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(
                    first_frame["payload"]
                        .as_str()
                        .expect("payload should be a string")
                )
                .expect("snapshot payload should decode"),
            b"\x1b(0qqq\x1b(B"
        );

        let next_frame = tokio::time::timeout(Duration::from_millis(80), socket.next()).await;
        assert!(
            next_frame.is_err(),
            "idle pane attach should not emit a replace frame on the first poll"
        );

        let _ = socket.close(None).await;
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn pane_attach_route_rejects_when_attach_slots_are_exhausted() {
        let target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let fixture = PaneAttachFixture::new_with_state_override(
            "secret",
            vec![ExpectedSnapshotCall {
                target: target.clone(),
                preserve_ansi: true,
                snapshot: PaneSnapshot {
                    output: "ready".into(),
                    width: 80,
                    height: 24,
                },
            }],
            vec![PaneAttachLookup::Attachable(target.clone())],
            PaneAttachLookup::Attachable(target),
            PaneAttachFixture::exhausted_attach_slots,
        )
        .await;

        let url = format!("ws://{}/api/v1/panes/7/attach?token=secret", fixture.addr);
        let error = connect_async(url)
            .await
            .expect_err("exhausted attach slots should reject websocket upgrade");
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            }
            other => panic!("expected HTTP rejection, got {other:?}"),
        }

        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn pane_attach_route_retargets_same_socket_when_backing_session_changes() {
        let initial_target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session-a".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let replacement_target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session-b".into(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".into(),
                },
            },
        };
        let fixture = PaneAttachFixture::new(
            "secret",
            vec![
                ExpectedSnapshotCall {
                    target: initial_target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "\u{1b}[34msession-a\u{1b}[0m".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: initial_target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "session-a".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: replacement_target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "session-b".into(),
                        width: 120,
                        height: 40,
                    },
                },
            ],
            vec![
                PaneAttachLookup::Attachable(initial_target.clone()),
                PaneAttachLookup::Attachable(initial_target.clone()),
                PaneAttachLookup::Attachable(replacement_target.clone()),
            ],
            PaneAttachLookup::Attachable(replacement_target),
        )
        .await;

        let mut socket = fixture.connect_pane(7, "secret").await;

        let snapshot_frame = socket
            .next()
            .await
            .expect("snapshot frame should arrive")
            .expect("snapshot frame should be ok")
            .into_text()
            .expect("snapshot frame should be text");
        let snapshot_frame: Value =
            serde_json::from_str(&snapshot_frame).expect("snapshot frame should be valid json");
        assert_eq!(snapshot_frame["type"], serde_json::json!("snapshot"));
        assert_eq!(snapshot_frame["cols"], serde_json::json!(80));
        assert_eq!(snapshot_frame["rows"], serde_json::json!(24));

        let replace_frame = tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .expect("retargeted pane should emit a replace frame")
            .expect("replace frame should exist")
            .expect("replace frame should be ok")
            .into_text()
            .expect("replace frame should be text");
        let replace_frame: Value =
            serde_json::from_str(&replace_frame).expect("replace frame should be valid json");
        assert_eq!(replace_frame["type"], serde_json::json!("replace"));
        assert_eq!(replace_frame["payload"], serde_json::json!("session-b"));
        assert_eq!(replace_frame["cols"], serde_json::json!(120));
        assert_eq!(replace_frame["rows"], serde_json::json!(40));

        let _ = socket.send(TungsteniteMessage::Close(None)).await;
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn pane_attach_route_tolerates_transient_not_found_during_retarget_polling() {
        let target = PaneAttachTarget {
            tab_id: 1,
            pane_id: 7,
            kind: PaneAttachKind::Tmux {
                session_name: "taarof-session".into(),
                target: crate::tmux::TmuxTarget::Local,
            },
        };
        let fixture = PaneAttachFixture::new(
            "secret",
            vec![
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: true,
                    snapshot: PaneSnapshot {
                        output: "\u{1b}[32mready\u{1b}[0m".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready".into(),
                        width: 80,
                        height: 24,
                    },
                },
                ExpectedSnapshotCall {
                    target: target.clone(),
                    preserve_ansi: false,
                    snapshot: PaneSnapshot {
                        output: "ready again".into(),
                        width: 80,
                        height: 24,
                    },
                },
            ],
            vec![
                PaneAttachLookup::Attachable(target.clone()),
                PaneAttachLookup::Attachable(target.clone()),
                PaneAttachLookup::NotFound,
                PaneAttachLookup::Attachable(target.clone()),
            ],
            PaneAttachLookup::Attachable(target),
        )
        .await;

        let mut socket = fixture.connect_pane(7, "secret").await;

        let snapshot_frame = socket
            .next()
            .await
            .expect("snapshot frame should arrive")
            .expect("snapshot frame should be ok")
            .into_text()
            .expect("snapshot frame should be text");
        let snapshot_frame: Value =
            serde_json::from_str(&snapshot_frame).expect("snapshot frame should be valid json");
        assert_eq!(snapshot_frame["type"], serde_json::json!("snapshot"));

        let transient_frame = tokio::time::timeout(Duration::from_millis(30), socket.next()).await;
        assert!(
            transient_frame.is_err(),
            "transient not-found lookup should not emit an error or close frame"
        );

        let replace_frame = tokio::time::timeout(Duration::from_millis(80), socket.next())
            .await
            .expect("recovered pane should emit a replace frame")
            .expect("replace frame should exist")
            .expect("replace frame should be ok")
            .into_text()
            .expect("replace frame should be text");
        let replace_frame: Value =
            serde_json::from_str(&replace_frame).expect("replace frame should be valid json");
        assert_eq!(replace_frame["type"], serde_json::json!("replace"));
        assert_eq!(replace_frame["payload"], serde_json::json!("ready again"));

        let _ = socket.send(TungsteniteMessage::Close(None)).await;
        fixture.shutdown().await;
    }

    #[test]
    fn resolve_bind_addr_allows_loopback_without_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 7800,
            bind_address: "127.0.0.1".into(),
            unsafe_allow_non_loopback: false,
        };

        assert_eq!(
            resolve_bind_addr(&config).expect("loopback bind should be allowed"),
            SocketAddr::from(([127, 0, 0, 1], 7800))
        );
    }

    #[test]
    fn resolve_bind_addr_rejects_non_loopback_without_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 7800,
            bind_address: "0.0.0.0".into(),
            unsafe_allow_non_loopback: false,
        };

        let error = resolve_bind_addr(&config).expect_err("remote bind should be rejected");
        assert!(error.contains("refusing to bind HTTP API to non-loopback address"));
        assert!(error.contains(HTTP_REMOTE_BIND_OPT_IN));
    }

    #[test]
    fn resolve_bind_addr_allows_non_loopback_with_explicit_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 9090,
            bind_address: "0.0.0.0".into(),
            unsafe_allow_non_loopback: true,
        };

        assert_eq!(
            resolve_bind_addr(&config).expect("opted-in remote bind should be allowed"),
            SocketAddr::from(([0, 0, 0, 0], 9090))
        );
    }

    #[test]
    fn resolve_bind_addr_rejects_invalid_bind_address() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 7800,
            bind_address: "localhost".into(),
            unsafe_allow_non_loopback: false,
        };

        let error = resolve_bind_addr(&config).expect_err("invalid bind_address should fail");
        assert!(error.contains("invalid [http].bind_address"));
    }

    #[test]
    fn resolve_bind_addr_allows_ipv6_loopback_without_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 7800,
            bind_address: "::1".into(),
            unsafe_allow_non_loopback: false,
        };

        let expected = SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 7800);
        assert_eq!(
            resolve_bind_addr(&config).expect("ipv6 loopback bind should be allowed"),
            expected
        );
    }

    #[test]
    fn resolve_bind_addr_rejects_ipv6_wildcard_without_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 7800,
            bind_address: "::".into(),
            unsafe_allow_non_loopback: false,
        };

        let error = resolve_bind_addr(&config).expect_err("ipv6 wildcard bind should be rejected");
        assert!(error.contains("refusing to bind HTTP API to non-loopback address"));
        assert!(error.contains(HTTP_REMOTE_BIND_OPT_IN));
    }

    #[test]
    fn resolve_bind_addr_allows_ipv6_wildcard_with_opt_in() {
        let config = crate::config::HttpConfig {
            enabled: true,
            port: 9090,
            bind_address: "::".into(),
            unsafe_allow_non_loopback: true,
        };

        let expected = SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 9090);
        assert_eq!(
            resolve_bind_addr(&config).expect("opted-in ipv6 wildcard bind should be allowed"),
            expected
        );
    }
}
