//! The runtime adapter boundary and its two implementations.
//!
//! The gateway talks to exactly one taarof runtime through [`RuntimeAdapter`].
//! An adapter attaches a brokered pane (returning the broker's canonical
//! checkpoint plus a stream of ordered output), dispatches already-authorized
//! input and resize, and creates tabs and splits. It must never surface the
//! runtime's Unix socket path, bearer token, or PIDs to a caller.
//!
//! Two implementations exist:
//!
//! - [`FakeRuntimeAdapter`] — a genuine in-memory runtime used by the contract
//!   and relay tests. It maintains real per-pane screen state, produces
//!   reconstructable checkpoints, echoes dispatched input as ordered output, and
//!   models the "cannot cover the requested cursor → new epoch + fresh
//!   checkpoint" rule. It is not a mock: it exercises the same relay paths the
//!   HTTP adapter does.
//! - [`HttpRuntimeAdapter`] — a loopback client of the desktop app's PTY adapter
//!   WebSocket (`GET /api/v1/tabs/{tab}/panes/{pane}/pty/ws`) and its control
//!   POSTs. It reads the process-scoped bearer token *at the moment of use* and
//!   never persists it, and it verifies the live runtime's advertised identity
//!   before streaming a single byte.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::body::Bytes;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;

use crate::config::ResolvedRuntime;
use crate::error::GatewayError;

/// Logical tab identifier as understood by the runtime. Never a filesystem path.
pub type TabId = String;

/// Logical pane identifier as understood by the runtime. Never a filesystem path.
pub type PaneId = String;

/// Orientation for a new split relative to an existing pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

impl SplitDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// The identity a live runtime advertises on the wire: a stable instance id and
/// the named session. The gateway is pinned to exactly one such identity and
/// refuses to relay to any other (design spec, fail-closed on runtime change).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub instance_id: String,
    pub session_name: String,
}

impl RuntimeIdentity {
    pub fn new(instance_id: impl Into<String>, session_name: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            session_name: session_name.into(),
        }
    }
}

/// The broker's canonical ANSI checkpoint plus the dimensions and sequence it
/// represents. Feeding `ansi` into a fresh terminal model reconstructs the
/// screen before live output begins.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub epoch: String,
    pub output_seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub ansi: Vec<u8>,
    pub state_hash: String,
}

/// One ordered chunk of raw output for an attached pane.
#[derive(Clone, Debug)]
pub struct OutputChunk {
    pub epoch: String,
    pub output_seq: u64,
    pub bytes: Vec<u8>,
}

/// A resume request: the client's last epoch and acknowledged output sequence.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub epoch: String,
    pub output_seq: u64,
}

/// The result of attaching to a pane: the identity the runtime advertised, the
/// bootstrap checkpoint, and a stream of subsequent ordered output.
pub struct AttachResult {
    pub identity: RuntimeIdentity,
    pub checkpoint: Checkpoint,
    pub output: mpsc::Receiver<OutputChunk>,
}

/// Loopback boundary to the designated taarof runtime.
///
/// Implementations relay attach/input/resize/tab/split to the runtime's loopback
/// PTY adapter. `send_input`/`resize` carry bytes/dimensions the gateway has
/// *already authorized*; the adapter does not re-authorize, it only transports.
#[allow(async_fn_in_trait)]
pub trait RuntimeAdapter: Send + Sync {
    /// Attach to a pane, optionally resuming from `resume`. Returns the identity,
    /// the canonical checkpoint, and the live output stream.
    async fn attach(
        &self,
        tab: &str,
        pane: &str,
        resume: Option<Cursor>,
    ) -> Result<AttachResult, GatewayError>;

    /// Deliver already-authorized input bytes to an attached pane, returning the
    /// output sequence observed after final dispatch (the ack position).
    async fn send_input(&self, tab: &str, pane: &str, bytes: &[u8]) -> Result<u64, GatewayError>;

    /// Resize an attached pane, returning the post-dispatch ack position.
    async fn resize(
        &self,
        tab: &str,
        pane: &str,
        cols: u16,
        rows: u16,
    ) -> Result<u64, GatewayError>;

    /// Create a new tab, returning its identifier.
    async fn create_tab(&self) -> Result<TabId, GatewayError>;

    /// Split an existing pane, returning the new pane's identifier.
    async fn create_split(
        &self,
        tab: &str,
        pane: &str,
        direction: SplitDirection,
    ) -> Result<PaneId, GatewayError>;

    /// Release any resources held for an attached pane.
    async fn detach(&self, tab: &str, pane: &str);
}

fn new_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn state_hash(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

// ── Fake in-memory runtime ───────────────────────────────────────────────────

struct FakePane {
    epoch: String,
    output_seq: u64,
    cols: u16,
    rows: u16,
    /// Accumulated screen bytes, used to build a reconstructable checkpoint.
    screen: Vec<u8>,
    /// The output sender for the current attach, if any.
    live: Option<mpsc::Sender<OutputChunk>>,
}

impl FakePane {
    fn checkpoint(&self) -> Checkpoint {
        // A canonical redraw: reset the terminal, then re-emit the screen. Feeding
        // this into a fresh model reconstructs the rendered text.
        let mut ansi = Vec::with_capacity(self.screen.len() + 2);
        ansi.extend_from_slice(b"\x1bc");
        ansi.extend_from_slice(&self.screen);
        Checkpoint {
            epoch: self.epoch.clone(),
            output_seq: self.output_seq,
            cols: self.cols,
            rows: self.rows,
            state_hash: state_hash(&ansi),
            ansi,
        }
    }
}

/// A genuine in-memory runtime. Not a mock: it holds real per-pane state,
/// produces reconstructable checkpoints, and echoes input as ordered output.
pub struct FakeRuntimeAdapter {
    identity: RuntimeIdentity,
    panes: Mutex<HashMap<(String, String), FakePane>>,
    /// When set, any attach carrying a resume cursor is treated as uncoverable,
    /// forcing a new epoch and a fresh checkpoint. Models a runtime whose replay
    /// window has evicted the requested output.
    force_resume_gap: bool,
    split_counter: Mutex<u64>,
}

impl FakeRuntimeAdapter {
    pub fn new(identity: RuntimeIdentity) -> Self {
        Self {
            identity,
            panes: Mutex::new(HashMap::new()),
            force_resume_gap: false,
            split_counter: Mutex::new(0),
        }
    }

    /// Build a fake whose replay window never covers a resume, so every resume
    /// attach restarts with a new epoch and fresh checkpoint.
    pub fn with_forced_resume_gap(identity: RuntimeIdentity) -> Self {
        Self {
            force_resume_gap: true,
            ..Self::new(identity)
        }
    }

    fn key(tab: &str, pane: &str) -> (String, String) {
        (tab.to_string(), pane.to_string())
    }
}

impl RuntimeAdapter for FakeRuntimeAdapter {
    async fn attach(
        &self,
        tab: &str,
        pane: &str,
        resume: Option<Cursor>,
    ) -> Result<AttachResult, GatewayError> {
        let mut panes = self.panes.lock().expect("fake panes poisoned");
        let entry = panes
            .entry(Self::key(tab, pane))
            .or_insert_with(|| FakePane {
                epoch: new_epoch(),
                output_seq: 0,
                cols: 80,
                rows: 24,
                screen: b"taarof$ ".to_vec(),
                live: None,
            });

        // If a resume cannot be honored, start a new epoch rather than claim a
        // continuity we cannot prove.
        if let Some(cursor) = resume {
            let uncoverable = self.force_resume_gap
                || cursor.epoch != entry.epoch
                || cursor.output_seq > entry.output_seq;
            if uncoverable {
                entry.epoch = new_epoch();
            }
        }

        let (tx, rx) = mpsc::channel(64);
        entry.live = Some(tx);
        Ok(AttachResult {
            identity: self.identity.clone(),
            checkpoint: entry.checkpoint(),
            output: rx,
        })
    }

    async fn send_input(&self, tab: &str, pane: &str, bytes: &[u8]) -> Result<u64, GatewayError> {
        let mut panes = self.panes.lock().expect("fake panes poisoned");
        let entry = panes
            .get_mut(&Self::key(tab, pane))
            .ok_or_else(|| GatewayError::RuntimeUnavailable("pane is not attached".to_string()))?;
        entry.screen.extend_from_slice(bytes);
        entry.output_seq += 1;
        let seq = entry.output_seq;
        let epoch = entry.epoch.clone();
        if let Some(sender) = entry.live.as_ref() {
            // Echo the input as ordered output, as a real PTY would.
            let _ = sender.try_send(OutputChunk {
                epoch,
                output_seq: seq,
                bytes: bytes.to_vec(),
            });
        }
        Ok(seq)
    }

    async fn resize(
        &self,
        tab: &str,
        pane: &str,
        cols: u16,
        rows: u16,
    ) -> Result<u64, GatewayError> {
        let mut panes = self.panes.lock().expect("fake panes poisoned");
        let entry = panes
            .get_mut(&Self::key(tab, pane))
            .ok_or_else(|| GatewayError::RuntimeUnavailable("pane is not attached".to_string()))?;
        entry.cols = cols;
        entry.rows = rows;
        entry.output_seq += 1;
        Ok(entry.output_seq)
    }

    async fn create_tab(&self) -> Result<TabId, GatewayError> {
        Ok(format!("tab-{}", new_epoch()))
    }

    async fn create_split(
        &self,
        _tab: &str,
        _pane: &str,
        _direction: SplitDirection,
    ) -> Result<PaneId, GatewayError> {
        let mut counter = self.split_counter.lock().expect("split counter poisoned");
        *counter += 1;
        Ok(format!("pane-split-{counter}"))
    }

    async fn detach(&self, tab: &str, pane: &str) {
        if let Some(entry) = self
            .panes
            .lock()
            .expect("fake panes poisoned")
            .get_mut(&Self::key(tab, pane))
        {
            entry.live = None;
        }
    }
}

// ── HTTP/WebSocket runtime adapter ───────────────────────────────────────────

/// A live per-pane WebSocket connection to the desktop PTY adapter.
struct PaneConnection {
    write: AsyncMutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    >,
    epoch: String,
    /// Ack positions the reader task forwards after each dispatched control frame.
    acks: AsyncMutex<mpsc::Receiver<u64>>,
}

/// Loopback client of the desktop app's PTY adapter WebSocket and control POSTs.
///
/// Constructed with the *expected* runtime identity and a [`ResolvedRuntime`]
/// that names (but does not hold) the bearer-token file. The token is read fresh
/// on every attach and control POST — never cached in a field.
pub struct HttpRuntimeAdapter {
    /// Loopback authority, e.g. `127.0.0.1:7800`.
    authority: String,
    /// The runtime registry handle; its `read_bearer_token` is called at use.
    resolved: ResolvedRuntime,
    expected: RuntimeIdentity,
    /// Live per-pane connections held behind `Arc` so a dispatch can clone its
    /// handle out from under the map lock: the shared `connections` mutex is only
    /// ever held for the clone, never across the per-pane ack await. Without this
    /// one in-flight (or hung) dispatch would serialize every other device's
    /// attach/detach/input/resize behind the map lock.
    connections: AsyncMutex<HashMap<(String, String), Arc<PaneConnection>>>,
}

impl HttpRuntimeAdapter {
    pub fn new(
        authority: impl Into<String>,
        resolved: ResolvedRuntime,
        expected: RuntimeIdentity,
    ) -> Self {
        Self {
            authority: authority.into(),
            resolved,
            expected,
            connections: AsyncMutex::new(HashMap::new()),
        }
    }

    /// Validate the operator-selected identity before opening gateway listeners.
    /// Bound both wait time and response allocation; errors never contain tokens.
    pub async fn verify_identity(&self) -> Result<(), GatewayError> {
        tokio::time::timeout(Duration::from_secs(5), self.fetch_identity())
            .await
            .map_err(|_| GatewayError::RuntimeUnavailable("identity check timed out".into()))?
    }

    async fn fetch_identity(&self) -> Result<(), GatewayError> {
        use hyper::Request;
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::TokioExecutor;

        let token = self.bearer_token()?;
        let client: Client<_, http_body_util::Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let request = Request::builder()
            .uri(format!("http://{}/api/v1/runtime-identity", self.authority))
            .header("authorization", format!("Bearer {token}"))
            .body(http_body_util::Full::new(Bytes::new()))
            .map_err(|_| GatewayError::RuntimeProtocol("invalid identity request".into()))?;
        let response = client
            .request(request)
            .await
            .map_err(|_| GatewayError::RuntimeUnavailable("identity request failed".into()))?;
        if !response.status().is_success() {
            return Err(GatewayError::RuntimeUnavailable(
                "identity request rejected".into(),
            ));
        }
        let bytes = http_body_util::Limited::new(response.into_body(), 64 * 1024)
            .collect()
            .await
            .map_err(|_| {
                GatewayError::RuntimeProtocol("invalid or oversized identity response".into())
            })?
            .to_bytes();
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| GatewayError::RuntimeProtocol("identity response is not JSON".into()))?;
        let data = &value["data"];
        if value["ok"] != true || data["schema"] != "taarof.runtime-identity.v1" {
            return Err(GatewayError::RuntimeProtocol(
                "unsupported identity response".into(),
            ));
        }
        let instance = data["runtime_id"].as_str().filter(|v| !v.is_empty());
        let session = data["session_name"].as_str().filter(|v| !v.is_empty());
        let (Some(instance), Some(session)) = (instance, session) else {
            return Err(GatewayError::RuntimeProtocol(
                "missing live runtime identity".into(),
            ));
        };
        if instance != self.expected.instance_id || session != self.expected.session_name {
            return Err(GatewayError::RuntimeIdentityMismatch {
                expected_instance: self.expected.instance_id.clone(),
                expected_session: self.expected.session_name.clone(),
                observed_instance: instance.into(),
                observed_session: session.into(),
            });
        }
        Ok(())
    }

    /// Read the process-scoped bearer token at the moment of use. The value is
    /// never stored on `self`.
    fn bearer_token(&self) -> Result<String, GatewayError> {
        self.resolved.read_bearer_token()
    }

    fn deadline_ms() -> i64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now + 5_000
    }

    async fn control_post(&self, path: &str, body: Value) -> Result<Value, GatewayError> {
        use hyper::Request;
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::TokioExecutor;

        let token = self.bearer_token()?;
        let client: Client<_, http_body_util::Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let uri = format!("http://{}{}", self.authority, path);
        let request = Request::builder()
            .method("POST")
            .uri(&uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(http_body_util::Full::new(Bytes::from(body.to_string())))
            .map_err(|e| GatewayError::RuntimeProtocol(format!("building control request: {e}")))?;
        let response = client
            .request(request)
            .await
            .map_err(|e| GatewayError::RuntimeUnavailable(format!("control POST failed: {e}")))?;
        if !response.status().is_success() {
            return Err(GatewayError::RuntimeUnavailable(format!(
                "control POST returned status {}",
                response.status().as_u16()
            )));
        }
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| {
                GatewayError::RuntimeUnavailable(format!("reading control response: {e}"))
            })?
            .to_bytes();
        serde_json::from_slice(&bytes)
            .map_err(|e| GatewayError::RuntimeProtocol(format!("control response not JSON: {e}")))
    }
}

/// Extract the first present id among a set of candidate keys under `data`.
fn extract_id(data: &Value, keys: &[&str]) -> Option<String> {
    let data = data.get("data").unwrap_or(data);
    for key in keys {
        if let Some(value) = data.get(key) {
            if let Some(s) = value.as_str() {
                return Some(s.to_string());
            }
            if let Some(n) = value.as_u64() {
                return Some(n.to_string());
            }
        }
    }
    None
}

impl RuntimeAdapter for HttpRuntimeAdapter {
    async fn attach(
        &self,
        tab: &str,
        pane: &str,
        resume: Option<Cursor>,
    ) -> Result<AttachResult, GatewayError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let token = self.bearer_token()?;
        // Carry the bearer token in the Authorization header, never in the URL:
        // a query-string token can leak into connect-error text and logs. The
        // desktop adapter accepts either channel; only resume hints (non-secret)
        // stay in the query string.
        let mut url = format!(
            "ws://{}/api/v1/tabs/{tab}/panes/{pane}/pty/ws",
            self.authority
        );
        if let Some(cursor) = &resume {
            url.push_str(&format!(
                "?epoch={}&output_seq={}",
                cursor.epoch, cursor.output_seq
            ));
        }
        let mut request = url
            .into_client_request()
            .map_err(|e| GatewayError::RuntimeProtocol(format!("building attach request: {e}")))?;
        let bearer = format!("Bearer {token}")
            .parse()
            .map_err(|_| GatewayError::RuntimeProtocol("invalid bearer header".into()))?;
        request.headers_mut().insert("authorization", bearer);

        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| GatewayError::RuntimeUnavailable(format!("pty attach failed: {e}")))?;
        let (write, mut read) = socket.split();

        // The first frame must be the bootstrap checkpoint.
        let first = read
            .next()
            .await
            .ok_or_else(|| GatewayError::RuntimeProtocol("attach closed before checkpoint".into()))?
            .map_err(|e| GatewayError::RuntimeUnavailable(format!("attach read failed: {e}")))?;
        let text = first
            .to_text()
            .map_err(|_| GatewayError::RuntimeProtocol("checkpoint was not text".into()))?;
        let frame: Value = serde_json::from_str(text)
            .map_err(|e| GatewayError::RuntimeProtocol(format!("checkpoint not JSON: {e}")))?;

        let identity = RuntimeIdentity {
            instance_id: frame
                .get("runtime_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            session_name: frame
                .get("session_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        // Fail closed if the live runtime is not the one we are pinned to: refuse
        // to stream a single byte from an unexpected runtime.
        if identity != self.expected {
            return Err(GatewayError::RuntimeIdentityMismatch {
                expected_instance: self.expected.instance_id.clone(),
                expected_session: self.expected.session_name.clone(),
                observed_instance: identity.instance_id,
                observed_session: identity.session_name,
            });
        }

        let checkpoint = parse_checkpoint(&frame)?;
        let epoch = checkpoint.epoch.clone();

        let (output_tx, output_rx) = mpsc::channel(256);
        let (ack_tx, ack_rx) = mpsc::channel(64);
        let reader_epoch = epoch.clone();
        // A single reader task drains the socket: output frames flow to the relay,
        // ack frames unblock the matching send. It ends when the socket closes.
        tokio::spawn(async move {
            while let Some(message) = read.next().await {
                let Ok(message) = message else { break };
                let Ok(text) = message.to_text() else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let Ok(frame) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                match frame.get("kind").and_then(Value::as_str) {
                    Some("output") => {
                        if let Ok(chunk) = parse_output(&frame, &reader_epoch) {
                            if output_tx.send(chunk).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some("ack") => {
                        if let Some(seq) = frame
                            .get("ack_output_seq")
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse::<u64>().ok())
                        {
                            let _ = ack_tx.send(seq).await;
                        }
                    }
                    _ => {}
                }
            }
        });

        self.connections.lock().await.insert(
            (tab.to_string(), pane.to_string()),
            Arc::new(PaneConnection {
                write: AsyncMutex::new(write),
                epoch,
                acks: AsyncMutex::new(ack_rx),
            }),
        );

        Ok(AttachResult {
            identity,
            checkpoint,
            output: output_rx,
        })
    }

    async fn send_input(&self, tab: &str, pane: &str, bytes: &[u8]) -> Result<u64, GatewayError> {
        let frame = json!({
            "kind": "input",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": self.expected.instance_id,
            "session_name": self.expected.session_name,
            "tab_id": tab,
            "pane_id": pane,
            "input_seq": "1",
            "grant_generation": "1",
            "nonce": uuid::Uuid::new_v4().to_string(),
            "deadline_ms": Self::deadline_ms(),
            "payload_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            "byte_count": bytes.len(),
        });
        self.dispatch_and_await_ack(tab, pane, frame).await
    }

    async fn resize(
        &self,
        tab: &str,
        pane: &str,
        cols: u16,
        rows: u16,
    ) -> Result<u64, GatewayError> {
        let frame = json!({
            "kind": "resize",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": self.expected.instance_id,
            "session_name": self.expected.session_name,
            "tab_id": tab,
            "pane_id": pane,
            "input_seq": "1",
            "grant_generation": "1",
            "nonce": uuid::Uuid::new_v4().to_string(),
            "deadline_ms": Self::deadline_ms(),
            "cols": cols,
            "rows": rows,
        });
        self.dispatch_and_await_ack(tab, pane, frame).await
    }

    async fn create_tab(&self) -> Result<TabId, GatewayError> {
        let data = self
            .control_post("/api/v1/control/create-tab", json!({}))
            .await?;
        extract_id(&data, &["tab_id", "tab", "id"])
            .ok_or_else(|| GatewayError::RuntimeProtocol("create-tab returned no tab id".into()))
    }

    async fn create_split(
        &self,
        tab: &str,
        pane: &str,
        direction: SplitDirection,
    ) -> Result<PaneId, GatewayError> {
        let data = self
            .control_post(
                "/api/v1/control/split-pane",
                json!({ "tab": tab, "pane": pane, "direction": direction.as_str() }),
            )
            .await?;
        extract_id(&data, &["pane_id", "pane", "id"])
            .ok_or_else(|| GatewayError::RuntimeProtocol("split-pane returned no pane id".into()))
    }

    async fn detach(&self, tab: &str, pane: &str) {
        // Clone the handle out under the map lock, then release the map lock
        // before awaiting the close so a slow close never blocks other panes.
        let conn = self
            .connections
            .lock()
            .await
            .remove(&(tab.to_string(), pane.to_string()));
        if let Some(conn) = conn {
            let _ = conn.write.lock().await.send(Message::Close(None)).await;
        }
    }
}

impl HttpRuntimeAdapter {
    /// Send a control frame on the pane's live socket and await its ack.
    async fn dispatch_and_await_ack(
        &self,
        tab: &str,
        pane: &str,
        mut frame: Value,
    ) -> Result<u64, GatewayError> {
        // Clone the pane's connection handle out from under the shared map lock
        // and release that lock immediately: the ack await below must not hold
        // `connections`, or one hung pane would serialize every other pane and
        // device.
        let conn = {
            let connections = self.connections.lock().await;
            connections
                .get(&(tab.to_string(), pane.to_string()))
                .cloned()
                .ok_or_else(|| {
                    GatewayError::RuntimeUnavailable("pane is not attached".to_string())
                })?
        };
        // Stamp the live connection epoch so the desktop's epoch gate accepts it;
        // input is never carried across epochs.
        frame["epoch"] = json!(conn.epoch);
        conn.write
            .lock()
            .await
            .send(Message::text(frame.to_string()))
            .await
            .map_err(|e| GatewayError::RuntimeUnavailable(format!("control send failed: {e}")))?;
        let mut acks = conn.acks.lock().await;
        match tokio::time::timeout(Duration::from_secs(10), acks.recv()).await {
            Ok(Some(seq)) => Ok(seq),
            Ok(None) => Err(GatewayError::RuntimeUnavailable(
                "ack channel closed".into(),
            )),
            Err(_) => Err(GatewayError::RuntimeUnavailable(
                "timed out awaiting ack".into(),
            )),
        }
    }
}

fn parse_checkpoint(frame: &Value) -> Result<Checkpoint, GatewayError> {
    let checkpoint = frame
        .get("checkpoint")
        .ok_or_else(|| GatewayError::RuntimeProtocol("first frame was not a checkpoint".into()))?;
    let ansi_b64 = checkpoint
        .get("ansi_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::RuntimeProtocol("checkpoint missing ansi_base64".into()))?;
    let ansi = base64::engine::general_purpose::STANDARD
        .decode(ansi_b64)
        .map_err(|_| GatewayError::RuntimeProtocol("checkpoint ansi not base64".into()))?;
    Ok(Checkpoint {
        epoch: frame
            .get("epoch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        output_seq: frame
            .get("output_seq")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        cols: frame.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16,
        rows: frame.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16,
        state_hash: checkpoint
            .get("state_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ansi,
    })
}

fn parse_output(frame: &Value, epoch: &str) -> Result<OutputChunk, GatewayError> {
    let payload = frame
        .get("payload_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::RuntimeProtocol("output missing payload".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| GatewayError::RuntimeProtocol("output payload not base64".into()))?;
    Ok(OutputChunk {
        epoch: frame
            .get("epoch")
            .and_then(Value::as_str)
            .unwrap_or(epoch)
            .to_string(),
        output_seq: frame
            .get("output_seq")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        bytes,
    })
}
