//! Runtime-adapter and relay integration tests.
//!
//! These exercise the real adapters — the in-memory [`FakeRuntimeAdapter`] and
//! the loopback [`HttpRuntimeAdapter`] against a genuine local WebSocket/control
//! server — not mocks. They cover: the fake's real checkpoint/echo/resume
//! behavior; the relay's grant enforcement, fail-closed runtime-identity pin,
//! and metadata-only audit; and the HTTP adapter's identity verification and
//! read-the-token-at-use guarantee.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use taarof_control_gateway::audit::{AuditEvent, AuditSink};
use taarof_control_gateway::config::GatewayConfig;
use taarof_control_gateway::error::GatewayError;
use taarof_control_gateway::grants::{GrantError, Scope, CONTROL_OPERATIONS_PER_SECOND};
use taarof_control_gateway::runtime::OutputChunk;
use taarof_control_gateway::runtime::{
    Cursor, FakeRuntimeAdapter, HttpRuntimeAdapter, RuntimeAdapter, RuntimeIdentity, SplitDirection,
};
use taarof_control_gateway::terminal::{
    control_revoked_frame, grant_expiring_frame, heartbeat_frame, output_frame, validate_frame,
    ControlRevokedReason, NonceLedger, Relay, RelayError,
};

const NOW_MS: u64 = 1_600_000_000_000;

fn identity(instance: &str) -> RuntimeIdentity {
    RuntimeIdentity::new(instance, "primary")
}

/// A test audit sink that keeps every recorded event for inspection.
#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<AuditEvent>>>);

impl AuditSink for RecordingSink {
    fn record(&self, event: AuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl RecordingSink {
    fn events(&self) -> Vec<AuditEvent> {
        self.0.lock().unwrap().clone()
    }
}

// ── Fake runtime behavior ────────────────────────────────────────────────────

#[tokio::test]
async fn fake_attach_returns_a_reconstructable_checkpoint() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let attach = fake.attach("tab-1", "pane-1", None).await.unwrap();
    assert_eq!(attach.identity, identity("pinned"));
    // The checkpoint ansi is a canonical redraw carrying the current screen.
    let text = String::from_utf8_lossy(&attach.checkpoint.ansi);
    assert!(
        text.contains("taarof$"),
        "checkpoint must redraw the screen"
    );
    assert!(attach.checkpoint.state_hash.starts_with("sha256:"));
}

#[tokio::test]
async fn fake_input_is_echoed_as_ordered_output_and_acked() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let mut attach = fake.attach("tab-1", "pane-1", None).await.unwrap();

    let ack1 = fake
        .send_input("tab-1", "pane-1", b"echo hi\n")
        .await
        .unwrap();
    let ack2 = fake.send_input("tab-1", "pane-1", b"ls\n").await.unwrap();
    assert!(ack2 > ack1, "output sequence advances monotonically");

    let chunk = attach.output.recv().await.expect("first echo chunk");
    assert_eq!(chunk.bytes, b"echo hi\n");
    assert_eq!(chunk.output_seq, ack1);
}

#[tokio::test]
async fn fake_resume_beyond_window_starts_a_new_epoch() {
    let fake = FakeRuntimeAdapter::with_forced_resume_gap(identity("pinned"));
    let first = fake.attach("tab-1", "pane-1", None).await.unwrap();
    let original_epoch = first.checkpoint.epoch.clone();

    // A resume the window cannot cover must not claim continuity: it restarts
    // with a new epoch and a fresh checkpoint.
    let resumed = fake
        .attach(
            "tab-1",
            "pane-1",
            Some(Cursor {
                epoch: original_epoch.clone(),
                output_seq: 0,
            }),
        )
        .await
        .unwrap();
    assert_ne!(
        resumed.checkpoint.epoch, original_epoch,
        "an uncoverable resume must begin a new epoch"
    );
}

// ── Relay behavior ───────────────────────────────────────────────────────────

fn control_input_frame(epoch: &str, generation: u64, payload: &[u8]) -> Value {
    json!({
        "kind": "input",
        "protocol_version": { "major": 1, "minor": 0 },
        "runtime_id": "pinned",
        "session_name": "primary",
        "tab_id": "tab-1",
        "pane_id": "pane-1",
        "epoch": epoch,
        "input_seq": "1",
        "grant_generation": generation.to_string(),
        "nonce": uuid::Uuid::new_v4().to_string(),
        "deadline_ms": NOW_MS + 5_000,
        "payload_base64": base64::engine::general_purpose::STANDARD.encode(payload),
        "byte_count": payload.len(),
    })
}

#[tokio::test]
async fn relay_attach_emits_a_schema_valid_checkpoint_frame() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink.clone(), identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let attach = relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .expect("attach under observe");

    // The gateway-produced checkpoint frame must satisfy the protocol schema.
    let mut nonces = NonceLedger::new();
    validate_frame(&attach.checkpoint_frame, NOW_MS, &mut nonces)
        .expect("relay checkpoint frame must be protocol-valid");
    assert_eq!(attach.checkpoint_frame["kind"], json!("checkpoint"));
}

#[tokio::test]
async fn relay_refuses_to_relay_when_the_live_runtime_is_not_pinned() {
    // The runtime advertises "impostor"; the gateway is pinned to "pinned".
    let fake = FakeRuntimeAdapter::new(identity("impostor"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink.clone(), identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let err = relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .map(|_| ())
        .expect_err("a different runtime must be refused");
    assert!(
        matches!(
            err,
            RelayError::Runtime(GatewayError::RuntimeIdentityMismatch { .. })
        ),
        "expected a fail-closed identity mismatch, got {err:?}"
    );
}

#[tokio::test]
async fn relay_input_requires_pane_control_and_echoes_only_a_byte_count() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink.clone(), identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let attach = relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .unwrap();
    let epoch = attach.checkpoint_frame["epoch"]
        .as_str()
        .unwrap()
        .to_string();

    // Without a pane-control grant, input is denied.
    let denied = relay
        .relay_input(
            "device-1",
            &control_input_frame(&epoch, 0, b"nope\n"),
            NOW_MS,
        )
        .await
        .expect_err("observe alone cannot write");
    assert!(matches!(denied, RelayError::Grant(_)));

    // With a pane-control grant, the same input is accepted and acked.
    let generation = relay
        .grant_pane_control("device-1", "tab-1", "pane-1", NOW_MS)
        .unwrap();
    let secret = b"password123\n";
    let ack = relay
        .relay_input(
            "device-1",
            &control_input_frame(&epoch, generation, secret),
            NOW_MS,
        )
        .await
        .expect("authorized input is relayed");
    assert_eq!(ack["kind"], json!("ack"));

    // The audit trail recorded the input by byte count only — never the content.
    let events = sink.events();
    let input_event = events
        .iter()
        .find(|e| {
            e.action == taarof_control_gateway::audit::AuditAction::Input
                && e.outcome == taarof_control_gateway::audit::Outcome::Ok
        })
        .expect("an input event was audited");
    assert_eq!(input_event.byte_count, secret.len() as u64);
    let serialized = format!("{input_event:?}");
    assert!(
        !serialized.contains("password123"),
        "audit event leaked terminal content: {serialized}"
    );
}

#[tokio::test]
async fn relay_input_replayed_nonce_is_rejected() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink, identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let attach = relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .unwrap();
    let epoch = attach.checkpoint_frame["epoch"]
        .as_str()
        .unwrap()
        .to_string();
    let generation = relay
        .grant_pane_control("device-1", "tab-1", "pane-1", NOW_MS)
        .unwrap();

    let frame = control_input_frame(&epoch, generation, b"ls\n");
    relay
        .relay_input("device-1", &frame, NOW_MS)
        .await
        .expect("first use of the nonce is accepted");
    let replay = relay
        .relay_input("device-1", &frame, NOW_MS)
        .await
        .expect_err("the same nonce cannot be reused");
    assert!(
        matches!(
            replay,
            RelayError::Frame(taarof_control_gateway::terminal::FrameError::NonceReplay)
        ),
        "expected NonceReplay, got {replay:?}"
    );
}

#[tokio::test]
async fn relay_create_tab_requires_runtime_mutation() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink, identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let denied = relay
        .create_tab("device-1", NOW_MS)
        .await
        .expect_err("observe alone cannot create tabs");
    assert!(matches!(denied, RelayError::Grant(_)));

    relay.grant_runtime_mutation("device-1", NOW_MS).unwrap();
    let tab = relay
        .create_tab("device-1", NOW_MS)
        .await
        .expect("runtime mutation authorizes tab creation");
    assert!(tab.starts_with("tab-"));
}

#[tokio::test]
async fn relay_refuses_input_carrying_a_stale_epoch() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink, identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .unwrap();
    let generation = relay
        .grant_pane_control("device-1", "tab-1", "pane-1", NOW_MS)
        .unwrap();

    // A frame stamped with a different epoch than the live attach must be
    // refused: input is never carried across connection epochs.
    let stale_epoch = "abababab-abab-4bab-8bab-abababababab";
    let err = relay
        .relay_input(
            "device-1",
            &control_input_frame(stale_epoch, generation, b"x\n"),
            NOW_MS,
        )
        .await
        .expect_err("a stale-epoch input must be refused");
    assert!(matches!(err, RelayError::EpochMismatch), "got {err:?}");
}

#[tokio::test]
async fn relay_enforces_the_control_operation_rate_budget() {
    let fake = FakeRuntimeAdapter::new(identity("pinned"));
    let sink = RecordingSink::default();
    let relay = Relay::new(fake, sink.clone(), identity("pinned"));

    relay.open_observe("device-1", NOW_MS);
    let attach = relay
        .attach("device-1", "tab-1", "pane-1", NOW_MS, None)
        .await
        .unwrap();
    let epoch = attach.checkpoint_frame["epoch"]
        .as_str()
        .unwrap()
        .to_string();
    let generation = relay
        .grant_pane_control("device-1", "tab-1", "pane-1", NOW_MS)
        .unwrap();

    // Thirty control ops in the same second are admitted.
    for _ in 0..CONTROL_OPERATIONS_PER_SECOND {
        relay
            .relay_input(
                "device-1",
                &control_input_frame(&epoch, generation, b"x"),
                NOW_MS,
            )
            .await
            .expect("within the per-second control budget");
    }
    // The next one in the same second is rate limited.
    let limited = relay
        .relay_input(
            "device-1",
            &control_input_frame(&epoch, generation, b"x"),
            NOW_MS,
        )
        .await
        .expect_err("the 31st control op in the second is refused");
    assert!(matches!(
        limited,
        RelayError::Grant(GrantError::RateLimited)
    ));

    // The breach was audited as rate_limited, not as content.
    let events = sink.events();
    assert!(events.iter().any(|e| e.outcome
        == taarof_control_gateway::audit::Outcome::RateLimited
        && e.action == taarof_control_gateway::audit::AuditAction::Input));
}

/// Compile the published terminal-frame schema (with its `common.json` $refs
/// resolved) so emitted frames can be checked against the real contract the
/// Android/iOS clients consume — not just the gateway's own lax validator.
fn terminal_frame_schema() -> (boon::Schemas, boon::SchemaIndex) {
    let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/schemas");
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(schema_dir.join(name)).unwrap()).unwrap()
    };
    let mut compiler = boon::Compiler::new();
    compiler
        .add_resource(
            "https://taarof.onlyarag.com/protocol/schemas/common.json",
            read("common.json"),
        )
        .unwrap();
    compiler
        .add_resource(
            "https://taarof.onlyarag.com/protocol/schemas/terminal-frame.json",
            read("terminal-frame.json"),
        )
        .unwrap();
    let mut schemas = boon::Schemas::new();
    let index = compiler
        .compile(
            "https://taarof.onlyarag.com/protocol/schemas/terminal-frame.json",
            &mut schemas,
        )
        .unwrap();
    (schemas, index)
}

#[test]
fn emitted_lifecycle_frames_match_the_published_terminal_frame_schema() {
    // The real schema requires runtime_id to be a UUID, session_name/pane ids to
    // match their patterns, and enums to be exact — none of which the gateway's
    // own `validate_frame` enforces. Emit with a conformant identity and check.
    let id = RuntimeIdentity::new("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary");
    let chunk = OutputChunk {
        epoch: "6d43882a-7f60-40bb-9f6d-166f7b63d10d".to_string(),
        output_seq: 7,
        bytes: b"hello".to_vec(),
    };
    let epoch = &chunk.epoch;
    let frames = vec![
        ("output", output_frame(&id, "tab-1", "pane-1", &chunk)),
        (
            "heartbeat",
            heartbeat_frame(&id, "tab-1", "pane-1", epoch, 12_345),
        ),
        (
            "grant_expiring",
            grant_expiring_frame(
                &id,
                "tab-1",
                "pane-1",
                epoch,
                Scope::PaneControl,
                NOW_MS + 60_000,
            ),
        ),
        (
            "control_revoked",
            control_revoked_frame(
                &id,
                "tab-1",
                "pane-1",
                epoch,
                Scope::PaneControl,
                ControlRevokedReason::PaneChanged,
            ),
        ),
    ];

    let (schemas, index) = terminal_frame_schema();
    for (kind, frame) in frames {
        assert_eq!(frame["kind"], json!(kind));
        // The gateway's own validator (cheap, used on the hot path) still accepts it.
        let mut nonces = NonceLedger::new();
        validate_frame(&frame, NOW_MS, &mut nonces)
            .unwrap_or_else(|e| panic!("emitted {kind} frame failed the gateway validator: {e}"));
        // And it satisfies the published JSON Schema the clients interop against.
        schemas
            .validate(&frame, index)
            .unwrap_or_else(|e| panic!("emitted {kind} frame failed the published schema: {e}"));
    }
}

/// A non-UUID `runtime_id` is accepted by the gateway's lax validator but must be
/// rejected by the published schema — proving the schema check has teeth.
#[test]
fn published_schema_rejects_a_non_uuid_runtime_id() {
    let bad = RuntimeIdentity::new("not-a-uuid", "primary");
    let chunk = OutputChunk {
        epoch: "6d43882a-7f60-40bb-9f6d-166f7b63d10d".to_string(),
        output_seq: 1,
        bytes: b"x".to_vec(),
    };
    let frame = output_frame(&bad, "tab-1", "pane-1", &chunk);
    let (schemas, index) = terminal_frame_schema();
    assert!(
        schemas.validate(&frame, index).is_err(),
        "the published schema must reject a non-UUID runtime_id"
    );
}

// ── HTTP adapter against a local server ──────────────────────────────────────

mod http_server {
    use super::*;
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::extract::{Path as AxumPath, State};
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::net::SocketAddr;

    #[derive(Clone)]
    pub struct ServerState {
        pub runtime_id: String,
        pub session_name: String,
        pub epoch: String,
        /// The most recent bearer token the WS route saw, for the read-at-use
        /// test. Read from the Authorization header (the token is never in the URL).
        pub last_token: Arc<Mutex<Option<String>>>,
    }

    async fn pty_ws(
        AxumPath((_tab, pane)): AxumPath<(String, String)>,
        ws: WebSocketUpgrade,
        State(state): State<ServerState>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
            let token = auth.strip_prefix("Bearer ").unwrap_or(auth).to_string();
            *state.last_token.lock().unwrap() = Some(token);
        }
        // A pane whose id contains "silent" is never acked, so a test can hold one
        // dispatch pending and prove other panes are not serialized behind it.
        let should_ack = !pane.contains("silent");
        ws.on_upgrade(move |socket| drive_socket(socket, state, should_ack))
    }

    fn checkpoint_frame(state: &ServerState) -> String {
        let ansi = b"\x1bctaarof$ ";
        json!({
            "kind": "checkpoint",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": state.runtime_id,
            "session_name": state.session_name,
            "tab_id": "tab-1",
            "pane_id": "pane-1",
            "epoch": state.epoch,
            "output_seq": "0",
            "cols": 80,
            "rows": 24,
            "checkpoint": {
                "ansi_base64": base64::engine::general_purpose::STANDARD.encode(ansi),
                "byte_count": ansi.len(),
                "state_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            }
        })
        .to_string()
    }

    fn ack_frame(state: &ServerState, seq: u64) -> String {
        json!({
            "kind": "ack",
            "protocol_version": { "major": 1, "minor": 0 },
            "runtime_id": state.runtime_id,
            "session_name": state.session_name,
            "tab_id": "tab-1",
            "pane_id": "pane-1",
            "epoch": state.epoch,
            "ack_output_seq": seq.to_string(),
        })
        .to_string()
    }

    async fn drive_socket(mut socket: WebSocket, state: ServerState, should_ack: bool) {
        // Checkpoint first, exactly like the desktop adapter.
        if socket
            .send(Message::Text(checkpoint_frame(&state).into()))
            .await
            .is_err()
        {
            return;
        }
        let mut seq = 0u64;
        while let Some(Ok(message)) = socket.recv().await {
            if let Message::Text(text) = message {
                let frame: Value = match serde_json::from_str(text.as_str()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if frame.get("kind").and_then(Value::as_str) == Some("input") && should_ack {
                    seq += 1;
                    let _ = socket
                        .send(Message::Text(ack_frame(&state, seq).into()))
                        .await;
                }
            }
        }
    }

    async fn split_pane(Json(_body): Json<Value>) -> Json<Value> {
        Json(json!({ "ok": true, "data": { "pane_id": "pane-split-1" } }))
    }

    /// Spawn the local server and return its authority and shared state.
    pub async fn spawn(runtime_id: &str, session_name: &str) -> (String, ServerState) {
        let state = ServerState {
            runtime_id: runtime_id.to_string(),
            session_name: session_name.to_string(),
            epoch: "6d43882a-7f60-40bb-9f6d-166f7b63d10d".to_string(),
            last_token: Arc::new(Mutex::new(None)),
        };
        let app = Router::new()
            .route("/api/v1/tabs/{tab}/panes/{pane}/pty/ws", get(pty_ws))
            .route("/api/v1/control/split-pane", post(split_pane))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr.to_string(), state)
    }
}

/// Build a `ResolvedRuntime` pointing at a real registry + token file so the
/// adapter can read the bearer token at use.
fn resolved_runtime_with_token(
    dir: &std::path::Path,
    pid: u32,
    token: &str,
) -> taarof_control_gateway::config::ResolvedRuntime {
    let registry = dir.join("taarof-current.json");
    std::fs::write(
        &registry,
        json!({ "pid": pid, "socket_path": "/run/user/1000/taarof.sock" }).to_string(),
    )
    .unwrap();
    std::fs::write(dir.join(format!("taarof-http-{pid}.token")), token).unwrap();

    let toml = format!(
        r#"
[gateway]
bind_address = "127.0.0.1:0"
[runtime]
session_name = "primary"
instance_id = "pinned"
registry_path = "{}"
[database]
path = "/nonexistent/gateway.db"
"#,
        registry.display()
    );
    GatewayConfig::from_toml_str(&toml)
        .unwrap()
        .resolve_runtime()
        .unwrap()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "taarof-gw-adapter-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn http_adapter_attaches_and_verifies_matching_identity() {
    let (authority, _state) =
        http_server::spawn("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary").await;
    let dir = temp_dir("match");
    let resolved = resolved_runtime_with_token(&dir, 4242, "secret-token");

    let adapter = HttpRuntimeAdapter::new(
        authority,
        resolved,
        RuntimeIdentity::new("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary"),
    );
    let attach = adapter
        .attach("tab-1", "pane-1", None)
        .await
        .expect("attach");
    assert_eq!(attach.identity.session_name, "primary");
    assert!(String::from_utf8_lossy(&attach.checkpoint.ansi).contains("taarof$"));

    // A dispatched input is acked by the live socket.
    let ack = adapter
        .send_input("tab-1", "pane-1", b"ls\n")
        .await
        .expect("ack");
    assert!(ack >= 1);
}

#[tokio::test]
async fn http_adapter_fails_closed_on_wrong_runtime_identity() {
    // The live runtime advertises a different instance id than the gateway pins.
    let (authority, _state) = http_server::spawn("some-other-runtime", "primary").await;
    let dir = temp_dir("mismatch");
    let resolved = resolved_runtime_with_token(&dir, 4243, "secret-token");

    let adapter = HttpRuntimeAdapter::new(
        authority,
        resolved,
        RuntimeIdentity::new("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary"),
    );
    let err = adapter
        .attach("tab-1", "pane-1", None)
        .await
        .map(|_| ())
        .expect_err("a different runtime must be refused before streaming");
    assert!(matches!(err, GatewayError::RuntimeIdentityMismatch { .. }));
}

#[tokio::test]
async fn http_adapter_reads_the_bearer_token_at_use_not_once() {
    let (authority, state) =
        http_server::spawn("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary").await;
    let dir = temp_dir("token");
    let resolved = resolved_runtime_with_token(&dir, 4244, "token-A");

    let adapter = HttpRuntimeAdapter::new(
        authority,
        resolved,
        RuntimeIdentity::new("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary"),
    );
    adapter
        .attach("tab-1", "pane-1", None)
        .await
        .expect("first attach");
    assert_eq!(
        state.last_token.lock().unwrap().clone(),
        Some("token-A".to_string())
    );

    // Rotate the token on disk and re-attach: the adapter must read the new value,
    // proving it reads the token at use rather than caching it.
    std::fs::write(dir.join("taarof-http-4244.token"), "token-B").unwrap();
    adapter.detach("tab-1", "pane-1").await;
    adapter
        .attach("tab-1", "pane-2", None)
        .await
        .expect("second attach reads the rotated token");
    assert_eq!(
        state.last_token.lock().unwrap().clone(),
        Some("token-B".to_string())
    );
}

#[tokio::test]
async fn http_adapter_dispatch_does_not_serialize_across_panes() {
    // Regression guard for the shared-connections-mutex-across-await bug: a hung
    // dispatch on one pane must not block dispatches to other panes/devices.
    let (authority, _state) =
        http_server::spawn("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary").await;
    let dir = temp_dir("concurrent");
    let resolved = resolved_runtime_with_token(&dir, 4246, "secret-token");
    let adapter = Arc::new(HttpRuntimeAdapter::new(
        authority,
        resolved,
        RuntimeIdentity::new("018f0f1e-a2d3-4c55-8f7b-9023456789ab", "primary"),
    ));

    adapter
        .attach("tab-1", "silent-pane", None)
        .await
        .expect("attach the pane the server never acks");
    adapter
        .attach("tab-1", "fast-pane", None)
        .await
        .expect("attach the pane the server acks");

    // Hold one dispatch pending: the server never acks "silent-pane", so this
    // parks in its ack await.
    let hung = {
        let a = Arc::clone(&adapter);
        tokio::spawn(async move {
            let _ = a.send_input("tab-1", "silent-pane", b"x\n").await;
        })
    };
    // Let the hung dispatch reach its await (where the buggy code held the map lock).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A dispatch to a different pane must complete promptly rather than serialize
    // behind the hung one.
    let fast = tokio::time::timeout(
        Duration::from_secs(3),
        adapter.send_input("tab-1", "fast-pane", b"y\n"),
    )
    .await;
    assert!(
        matches!(fast, Ok(Ok(_))),
        "a dispatch to another pane must not block behind a hung pane: {fast:?}"
    );
    hung.abort();
}

#[tokio::test]
async fn http_adapter_create_split_parses_new_pane_id() {
    let (authority, _state) = http_server::spawn("pinned", "primary").await;
    let dir = temp_dir("split");
    let resolved = resolved_runtime_with_token(&dir, 4245, "secret-token");

    let adapter = HttpRuntimeAdapter::new(authority, resolved, identity("pinned"));
    let pane = adapter
        .create_split("tab-1", "pane-1", SplitDirection::Horizontal)
        .await
        .expect("split returns the new pane id");
    assert_eq!(pane, "pane-split-1");
}
