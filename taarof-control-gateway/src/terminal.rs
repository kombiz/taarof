//! Terminal-frame conformance validation, capability negotiation, and the
//! scoped relay that carries protocol frames between an authorized device and
//! the single configured runtime.
//!
//! Two concerns live here because they are one responsibility — speaking the
//! Taarof Remote Protocol terminal channel correctly:
//!
//! 1. **Validation** ([`validate_frame`], [`validate_negotiation`]) enforces the
//!    wire contract the fixtures in `protocol/` pin: required fields, the
//!    64 KiB control / 256 KiB output frame bounds, `byte_count` honesty,
//!    unsigned-64-bit sequence encoding, deadline freshness, and one-use nonces.
//!    The gateway rejects a malformed or replayed frame here before it can reach
//!    a grant check or the runtime.
//! 2. **Relay** ([`Relay`]) composes a [`crate::grants::GrantLedger`], a
//!    [`crate::runtime::RuntimeAdapter`], and a [`crate::audit::AuditSink`] to
//!    serve one device: it attaches panes under an observe grant, dispatches
//!    input/resize only under a live pane-control grant on the currently pinned
//!    epoch, and fails closed if the live runtime is not the configured one.
//!
//! Frame *content* never leaves this module into an audit record; only byte
//! counts and coarse action categories do (see [`crate::audit`]).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use base64::Engine;
use serde_json::{json, Value};

use crate::audit::{AuditAction, AuditEvent, AuditSink, Outcome};
use crate::grants::{GrantError, GrantLedger, PaneKey};
use crate::runtime::{
    Checkpoint, Cursor, OutputChunk, RuntimeAdapter, RuntimeIdentity, SplitDirection,
};

/// Output frames (runtime → device) are capped at 256 KiB by the protocol.
pub const OUTPUT_FRAME_MAX_BYTES: u64 = 262_144;
/// Control frames (device → runtime: input/resize/paste) are capped at 64 KiB.
pub const CONTROL_FRAME_MAX_BYTES: u64 = 65_536;

/// Signed frames whose deadline is older than this are rejected as replays.
pub const MAX_DEADLINE_AGE_MS: u64 = 30_000;

/// Capability names this protocol version understands. A negotiation naming any
/// other capability is rejected rather than silently ignored, so a client
/// cannot smuggle an unrecognized (e.g. platform-specific) behavior past the
/// gateway.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "biometric_user_verification",
    "raw_pty",
    "legacy_snapshot",
    "runtime_mutation",
];

/// Why a terminal frame or negotiation was rejected. Messages are metadata only
/// and deliberately match the substrings the protocol fixtures assert, so the
/// gateway and the published contract cannot drift apart unnoticed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    MissingField(&'static str),
    MissingEpoch,
    MissingNonce,
    MissingDeadline,
    DeadlineShape,
    DeadlineStale,
    NonceReplay,
    OutputByteCountOverLimit(u64),
    ControlByteCountOverLimit(u64),
    ControlDecodedOverLimit(u64),
    ByteCountMismatch { field: &'static str },
    InvalidBase64(&'static str),
    InvalidUint64(&'static str),
    UnknownKind(String),
    UnsupportedCapability(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing {name}"),
            Self::MissingEpoch => f.write_str("missing epoch"),
            Self::MissingNonce => f.write_str("missing nonce"),
            Self::MissingDeadline => f.write_str("missing deadline_ms"),
            Self::DeadlineShape => {
                f.write_str("deadline_ms must be an integer Unix millisecond timestamp")
            }
            Self::DeadlineStale => f.write_str("deadline stale"),
            Self::NonceReplay => f.write_str("nonce has already been used"),
            Self::OutputByteCountOverLimit(_) => {
                write!(f, "output byte_count exceeds {OUTPUT_FRAME_MAX_BYTES}")
            }
            Self::ControlByteCountOverLimit(_) => {
                write!(f, "control byte_count exceeds {CONTROL_FRAME_MAX_BYTES}")
            }
            Self::ControlDecodedOverLimit(_) => {
                write!(
                    f,
                    "control decoded payload exceeds {CONTROL_FRAME_MAX_BYTES}"
                )
            }
            Self::ByteCountMismatch { field } => {
                write!(
                    f,
                    "{field} byte_count does not match decoded payload length"
                )
            }
            Self::InvalidBase64(field) => write!(f, "{field} is not valid base64"),
            Self::InvalidUint64(field) => {
                write!(f, "{field} must be an unsigned 64-bit decimal string")
            }
            Self::UnknownKind(kind) => write!(f, "unknown terminal frame kind: {kind}"),
            Self::UnsupportedCapability(name) => {
                write!(f, "unsupported capability name: {name}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// One-use nonce ledger scoped to a single live connection. Unlike the signed
/// REST/WS cache in [`crate::auth`], connection nonces are retained for the
/// connection's whole lifetime (not pruned by deadline), because the protocol
/// requires input identifiers to be unique for the connection.
#[derive(Default)]
pub struct NonceLedger {
    seen: HashSet<String>,
}

impl NonceLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `nonce`, returning `true` if it was fresh and `false` if it was
    /// already used on this connection.
    pub fn consume(&mut self, nonce: &str) -> bool {
        self.seen.insert(nonce.to_string())
    }
}

/// Validate one terminal frame against the wire contract, consuming its nonce
/// (for input/resize) so a replay within the connection is rejected.
///
/// This is deliberately structural: it validates the frame's shape and bounds,
/// not whether the sender is *authorized* to send it — scope enforcement is the
/// relay's job. Ordering matters and mirrors the fixtures: presence, then shape,
/// then declared bounds, then decoded bounds, then freshness, then replay.
pub fn validate_frame(
    frame: &Value,
    now_ms: u64,
    nonces: &mut NonceLedger,
) -> Result<(), FrameError> {
    let kind = frame
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(FrameError::MissingField("kind"))?;

    // Every frame carries the connection epoch; its absence is the first thing
    // the protocol pins.
    require_epoch(frame)?;

    match kind {
        "checkpoint" => validate_checkpoint(frame),
        "output" => validate_output(frame),
        "input" => validate_control(frame, true, now_ms, nonces),
        "resize" => validate_control(frame, false, now_ms, nonces),
        "ack" => {
            require_uint64(frame, "ack_output_seq")?;
            Ok(())
        }
        "heartbeat" | "error" | "grant_expiring" | "control_revoked" => Ok(()),
        other => Err(FrameError::UnknownKind(other.to_string())),
    }
}

fn require_epoch(frame: &Value) -> Result<(), FrameError> {
    match frame.get("epoch").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => Ok(()),
        _ => Err(FrameError::MissingEpoch),
    }
}

fn validate_checkpoint(frame: &Value) -> Result<(), FrameError> {
    require_uint64(frame, "output_seq")?;
    let checkpoint = frame
        .get("checkpoint")
        .ok_or(FrameError::MissingField("checkpoint"))?;
    let ansi = checkpoint
        .get("ansi_base64")
        .and_then(Value::as_str)
        .ok_or(FrameError::MissingField("checkpoint.ansi_base64"))?;
    let byte_count = checkpoint
        .get("byte_count")
        .and_then(Value::as_u64)
        .ok_or(FrameError::MissingField("checkpoint.byte_count"))?;
    let decoded = decode_base64(ansi, "checkpoint.ansi_base64")?;
    if decoded.len() as u64 > OUTPUT_FRAME_MAX_BYTES {
        return Err(FrameError::OutputByteCountOverLimit(decoded.len() as u64));
    }
    if decoded.len() as u64 != byte_count {
        return Err(FrameError::ByteCountMismatch {
            field: "checkpoint.ansi_base64",
        });
    }
    Ok(())
}

fn validate_output(frame: &Value) -> Result<(), FrameError> {
    require_uint64(frame, "output_seq")?;
    let byte_count = frame
        .get("byte_count")
        .and_then(Value::as_u64)
        .ok_or(FrameError::MissingField("byte_count"))?;
    if byte_count > OUTPUT_FRAME_MAX_BYTES {
        return Err(FrameError::OutputByteCountOverLimit(byte_count));
    }
    let payload = frame
        .get("payload_base64")
        .and_then(Value::as_str)
        .ok_or(FrameError::MissingField("payload_base64"))?;
    let decoded = decode_base64(payload, "payload_base64")?;
    if decoded.len() as u64 > OUTPUT_FRAME_MAX_BYTES {
        return Err(FrameError::OutputByteCountOverLimit(decoded.len() as u64));
    }
    if decoded.len() as u64 != byte_count {
        return Err(FrameError::ByteCountMismatch {
            field: "payload_base64",
        });
    }
    Ok(())
}

/// Validate an input (`has_payload = true`) or resize (`false`) control frame.
fn validate_control(
    frame: &Value,
    has_payload: bool,
    now_ms: u64,
    nonces: &mut NonceLedger,
) -> Result<(), FrameError> {
    let nonce = frame
        .get("nonce")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or(FrameError::MissingNonce)?
        .to_string();

    // A present-but-non-integer deadline is a distinct error from an absent one,
    // because a string deadline is a client bug the protocol calls out by name.
    let deadline = match frame.get("deadline_ms") {
        None => return Err(FrameError::MissingDeadline),
        Some(Value::Null) => return Err(FrameError::MissingDeadline),
        Some(v) => v.as_u64().ok_or(FrameError::DeadlineShape)?,
    };

    require_uint64(frame, "input_seq")?;
    require_uint64(frame, "grant_generation")?;

    if has_payload {
        let byte_count = frame
            .get("byte_count")
            .and_then(Value::as_u64)
            .ok_or(FrameError::MissingField("byte_count"))?;
        // Declared bound first: an oversized declared count is rejected before we
        // spend work decoding the (possibly huge) payload.
        if byte_count > CONTROL_FRAME_MAX_BYTES {
            return Err(FrameError::ControlByteCountOverLimit(byte_count));
        }
        let payload = frame
            .get("payload_base64")
            .and_then(Value::as_str)
            .ok_or(FrameError::MissingField("payload_base64"))?;
        let decoded = decode_base64(payload, "payload_base64")?;
        // Decoded bound before the mismatch check: a payload that decodes over
        // the limit is an over-limit error even when its declared count lies.
        if decoded.len() as u64 > CONTROL_FRAME_MAX_BYTES {
            return Err(FrameError::ControlDecodedOverLimit(decoded.len() as u64));
        }
        if decoded.len() as u64 != byte_count {
            return Err(FrameError::ByteCountMismatch {
                field: "payload_base64",
            });
        }
    } else {
        // Resize frames still carry logical dimensions; their presence and range
        // is validated at dispatch, not here.
    }

    if now_ms > deadline.saturating_add(MAX_DEADLINE_AGE_MS) {
        return Err(FrameError::DeadlineStale);
    }

    if !nonces.consume(&nonce) {
        return Err(FrameError::NonceReplay);
    }
    Ok(())
}

/// Validate a capability-negotiation request: protocol major 1 and every named
/// capability drawn from [`KNOWN_CAPABILITIES`].
pub fn validate_negotiation(frame: &Value) -> Result<(), FrameError> {
    let major = frame
        .get("protocol_version")
        .and_then(|v| v.get("major"))
        .and_then(Value::as_u64)
        .ok_or(FrameError::MissingField("protocol_version.major"))?;
    if major != 1 {
        return Err(FrameError::UnknownKind(format!("protocol major {major}")));
    }
    let capabilities = frame
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or(FrameError::MissingField("capabilities"))?;
    for capability in capabilities {
        let name = capability
            .as_str()
            .ok_or(FrameError::MissingField("capabilities[]"))?;
        if !KNOWN_CAPABILITIES.contains(&name) {
            return Err(FrameError::UnsupportedCapability(name.to_string()));
        }
    }
    Ok(())
}

/// A `Uint64DecimalString` field must be present, a string, and parse as u64.
fn require_uint64(frame: &Value, field: &'static str) -> Result<u64, FrameError> {
    let raw = frame
        .get(field)
        .and_then(Value::as_str)
        .ok_or(FrameError::MissingField(field))?;
    raw.parse::<u64>()
        .map_err(|_| FrameError::InvalidUint64(field))
}

fn decode_base64(value: &str, field: &'static str) -> Result<Vec<u8>, FrameError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| FrameError::InvalidBase64(field))
}

// ── Relay ────────────────────────────────────────────────────────────────────

/// Why a relayed operation was refused. Distinguishes a malformed/replayed frame
/// (`Frame`), an unauthorized one (`Grant`), and a runtime-side failure
/// (`Runtime`) — including the fail-closed refusal when the live runtime is not
/// the pinned one.
#[derive(Debug)]
pub enum RelayError {
    Frame(FrameError),
    Grant(GrantError),
    Runtime(crate::error::GatewayError),
    /// A control frame carried an epoch other than the pane's live attach epoch.
    /// Input is never carried across connection epochs.
    EpochMismatch,
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "frame rejected: {e}"),
            Self::Grant(e) => write!(f, "grant denied: {e}"),
            Self::Runtime(e) => write!(f, "runtime: {e}"),
            Self::EpochMismatch => {
                f.write_str("control frame epoch does not match the live connection epoch")
            }
        }
    }
}

impl std::error::Error for RelayError {}

impl From<FrameError> for RelayError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}
impl From<GrantError> for RelayError {
    fn from(e: GrantError) -> Self {
        Self::Grant(e)
    }
}
impl From<crate::error::GatewayError> for RelayError {
    fn from(e: crate::error::GatewayError) -> Self {
        Self::Runtime(e)
    }
}

/// A successful attach: the bootstrap checkpoint frame to send the device, and
/// the live output stream to pump to it as ordered `output` frames.
pub struct Attachment {
    pub checkpoint_frame: Value,
    pub output: tokio::sync::mpsc::Receiver<OutputChunk>,
}

/// The scoped terminal relay. Composes grant policy, a runtime adapter, and a
/// metadata-only audit sink to serve authorized devices, and refuses to relay if
/// the live runtime is not the configured one.
pub struct Relay<A: RuntimeAdapter, S: AuditSink> {
    adapter: A,
    audit: S,
    expected: RuntimeIdentity,
    grants: Mutex<GrantLedger>,
    nonces: Mutex<HashMap<String, NonceLedger>>,
    /// The live connection epoch per attached (device, tab, pane). Input is only
    /// dispatched on the epoch the pane was attached under; a frame carrying any
    /// other epoch is refused so input is never carried across connection epochs.
    epochs: Mutex<HashMap<(String, String, String), String>>,
}

impl<A: RuntimeAdapter, S: AuditSink> Relay<A, S> {
    pub fn new(adapter: A, audit: S, expected: RuntimeIdentity) -> Self {
        Self {
            adapter,
            audit,
            expected,
            grants: Mutex::new(GrantLedger::new()),
            nonces: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
        }
    }

    fn record_epoch(&self, device: &str, tab: &str, pane: &str, epoch: &str) {
        self.epochs.lock().expect("epochs poisoned").insert(
            (device.to_string(), tab.to_string(), pane.to_string()),
            epoch.to_string(),
        );
    }

    /// Confirm a control frame carries the epoch the pane was attached under.
    /// Input is never carried across connection epochs.
    fn check_epoch(
        &self,
        device: &str,
        tab: &str,
        pane: &str,
        epoch: &str,
    ) -> Result<(), RelayError> {
        let epochs = self.epochs.lock().expect("epochs poisoned");
        match epochs.get(&(device.to_string(), tab.to_string(), pane.to_string())) {
            Some(live) if live == epoch => Ok(()),
            _ => Err(RelayError::EpochMismatch),
        }
    }

    /// Open (or renew) a device's observe grant.
    pub fn open_observe(&self, device: &str, now_ms: u64) {
        self.grants
            .lock()
            .expect("grants poisoned")
            .open_observe(device, now_ms);
        self.audit
            .record(AuditEvent::action(AuditAction::ObserveOpen, Outcome::Ok).device(device));
    }

    fn frame_context(&self, tab: &str, pane: &str, epoch: &str) -> FrameContext {
        FrameContext {
            runtime_id: self.expected.instance_id.clone(),
            session_name: self.expected.session_name.clone(),
            tab: tab.to_string(),
            pane: pane.to_string(),
            epoch: epoch.to_string(),
        }
    }

    /// Charge one metadata request against the device's budget.
    fn charge_metadata_rate(&self, device: &str, now_ms: u64) -> Result<(), RelayError> {
        self.grants
            .lock()
            .expect("grants poisoned")
            .check_metadata_rate(device, now_ms)
            .map_err(RelayError::Grant)
    }

    /// Charge one control operation against the device's budget.
    fn charge_control_rate(&self, device: &str, now_ms: u64) -> Result<(), RelayError> {
        self.grants
            .lock()
            .expect("grants poisoned")
            .check_control_rate(device, now_ms)
            .map_err(RelayError::Grant)
    }

    /// Attach a pane under the device's observe grant. Enforces the metadata rate
    /// budget and four-attach limit, verifies the live runtime is the pinned one
    /// (fail closed), and returns the checkpoint frame plus the live output stream.
    pub async fn attach(
        &self,
        device: &str,
        tab: &str,
        pane: &str,
        now_ms: u64,
        resume: Option<Cursor>,
    ) -> Result<Attachment, RelayError> {
        if let Err(e) = self.charge_metadata_rate(device, now_ms) {
            self.audit.record(
                AuditEvent::action(AuditAction::Attach, Outcome::RateLimited)
                    .device(device)
                    .pane(tab, pane),
            );
            return Err(e);
        }
        let pane_key = PaneKey::new(tab, pane);
        // Grant check first, under a short lock we never hold across an await.
        if let Err(e) = self
            .grants
            .lock()
            .expect("grants poisoned")
            .attach_pane(device, now_ms, &pane_key)
        {
            self.audit.record(
                AuditEvent::action(AuditAction::Attach, Outcome::Denied)
                    .device(device)
                    .pane(tab, pane),
            );
            return Err(e.into());
        }

        let attach = match self.adapter.attach(tab, pane, resume).await {
            Ok(attach) => attach,
            Err(e) => {
                self.grants
                    .lock()
                    .expect("grants poisoned")
                    .detach_pane(device, &pane_key);
                self.audit.record(
                    AuditEvent::action(AuditAction::Attach, Outcome::Error)
                        .device(device)
                        .pane(tab, pane),
                );
                return Err(e.into());
            }
        };

        // Fail closed if the live runtime is not the pinned one.
        if attach.identity != self.expected {
            self.grants
                .lock()
                .expect("grants poisoned")
                .detach_pane(device, &pane_key);
            self.audit.record(
                AuditEvent::action(AuditAction::Attach, Outcome::Denied)
                    .device(device)
                    .pane(tab, pane),
            );
            return Err(RelayError::Runtime(
                crate::error::GatewayError::RuntimeIdentityMismatch {
                    expected_instance: self.expected.instance_id.clone(),
                    expected_session: self.expected.session_name.clone(),
                    observed_instance: attach.identity.instance_id,
                    observed_session: attach.identity.session_name,
                },
            ));
        }

        // Pin the epoch this pane is now live on; later control frames must match.
        self.record_epoch(device, tab, pane, &attach.checkpoint.epoch);
        let ctx = self.frame_context(tab, pane, &attach.checkpoint.epoch);
        let checkpoint_frame = ctx.checkpoint_frame(&attach.checkpoint);
        self.audit.record(
            AuditEvent::action(AuditAction::Attach, Outcome::Ok)
                .device(device)
                .pane(tab, pane),
        );
        Ok(Attachment {
            checkpoint_frame,
            output: attach.output,
        })
    }

    /// Grant pane control for an attached pane; returns the control generation the
    /// device must echo in every input/resize frame.
    pub fn grant_pane_control(
        &self,
        device: &str,
        tab: &str,
        pane: &str,
        now_ms: u64,
    ) -> Result<u64, RelayError> {
        let result = self
            .grants
            .lock()
            .expect("grants poisoned")
            .grant_pane_control(device, now_ms, &PaneKey::new(tab, pane));
        let outcome = if result.is_ok() {
            Outcome::Ok
        } else {
            Outcome::Denied
        };
        self.audit.record(
            AuditEvent::action(AuditAction::PaneControlGrant, outcome)
                .device(device)
                .pane(tab, pane),
        );
        Ok(result?)
    }

    /// Grant runtime mutation (needed for tab/split creation).
    pub fn grant_runtime_mutation(&self, device: &str, now_ms: u64) -> Result<u64, RelayError> {
        let result = self
            .grants
            .lock()
            .expect("grants poisoned")
            .grant_runtime_mutation(device, now_ms);
        let outcome = if result.is_ok() {
            Outcome::Ok
        } else {
            Outcome::Denied
        };
        self.audit
            .record(AuditEvent::action(AuditAction::RuntimeMutationGrant, outcome).device(device));
        Ok(result?)
    }

    /// Relay a validated, authorized `input` frame to the runtime and return the
    /// `ack` frame. Order: structural validation and nonce consumption, then the
    /// grant/epoch/generation check, then dispatch, then audit (byte count only).
    pub async fn relay_input(
        &self,
        device: &str,
        frame: &Value,
        now_ms: u64,
    ) -> Result<Value, RelayError> {
        self.validate_device_frame(device, frame, now_ms)?;
        let (tab, pane) = frame_pane(frame)?;
        let epoch = frame_str(frame, "epoch")?;
        let generation = frame_uint(frame, "grant_generation")?;

        if let Err(e) = self.check_epoch(device, &tab, &pane, &epoch) {
            self.audit.record(
                AuditEvent::action(AuditAction::Input, Outcome::Denied)
                    .device(device)
                    .pane(&tab, &pane),
            );
            return Err(e);
        }

        if let Err(e) = self.charge_control_rate(device, now_ms) {
            self.audit.record(
                AuditEvent::action(AuditAction::Input, Outcome::RateLimited)
                    .device(device)
                    .pane(&tab, &pane),
            );
            return Err(e);
        }

        self.grants
            .lock()
            .expect("grants poisoned")
            .use_pane_control(device, now_ms, &PaneKey::new(&tab, &pane), generation)
            .inspect_err(|_| {
                self.audit.record(
                    AuditEvent::action(AuditAction::Input, Outcome::Denied)
                        .device(device)
                        .pane(&tab, &pane),
                )
            })?;

        let payload = frame_str(frame, "payload_base64")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .map_err(|_| RelayError::Frame(FrameError::InvalidBase64("payload_base64")))?;

        let ack_seq = match self.adapter.send_input(&tab, &pane, &bytes).await {
            Ok(seq) => seq,
            Err(e) => {
                self.audit.record(
                    AuditEvent::action(AuditAction::Input, Outcome::Error)
                        .device(device)
                        .pane(&tab, &pane)
                        .bytes(bytes.len() as u64),
                );
                return Err(e.into());
            }
        };

        // Content is never audited — only its size.
        self.audit.record(
            AuditEvent::action(AuditAction::Input, Outcome::Ok)
                .device(device)
                .pane(&tab, &pane)
                .bytes(bytes.len() as u64),
        );
        Ok(self.frame_context(&tab, &pane, &epoch).ack_frame(ack_seq))
    }

    /// Relay a validated, authorized `resize` frame and return the `ack` frame.
    pub async fn relay_resize(
        &self,
        device: &str,
        frame: &Value,
        now_ms: u64,
    ) -> Result<Value, RelayError> {
        self.validate_device_frame(device, frame, now_ms)?;
        let (tab, pane) = frame_pane(frame)?;
        let epoch = frame_str(frame, "epoch")?;
        let generation = frame_uint(frame, "grant_generation")?;
        let cols = frame_dim(frame, "cols")?;
        let rows = frame_dim(frame, "rows")?;

        if let Err(e) = self.check_epoch(device, &tab, &pane, &epoch) {
            self.audit.record(
                AuditEvent::action(AuditAction::Resize, Outcome::Denied)
                    .device(device)
                    .pane(&tab, &pane),
            );
            return Err(e);
        }

        if let Err(e) = self.charge_control_rate(device, now_ms) {
            self.audit.record(
                AuditEvent::action(AuditAction::Resize, Outcome::RateLimited)
                    .device(device)
                    .pane(&tab, &pane),
            );
            return Err(e);
        }

        self.grants
            .lock()
            .expect("grants poisoned")
            .use_pane_control(device, now_ms, &PaneKey::new(&tab, &pane), generation)
            .inspect_err(|_| {
                self.audit.record(
                    AuditEvent::action(AuditAction::Resize, Outcome::Denied)
                        .device(device)
                        .pane(&tab, &pane),
                )
            })?;

        let ack_seq = self.adapter.resize(&tab, &pane, cols, rows).await?;
        self.audit.record(
            AuditEvent::action(AuditAction::Resize, Outcome::Ok)
                .device(device)
                .pane(&tab, &pane),
        );
        Ok(self.frame_context(&tab, &pane, &epoch).ack_frame(ack_seq))
    }

    /// Create a tab under a live runtime-mutation grant.
    pub async fn create_tab(&self, device: &str, now_ms: u64) -> Result<String, RelayError> {
        if let Err(e) = self.charge_control_rate(device, now_ms) {
            self.audit.record(
                AuditEvent::action(AuditAction::TabCreate, Outcome::RateLimited).device(device),
            );
            return Err(e);
        }
        self.grants
            .lock()
            .expect("grants poisoned")
            .use_runtime_mutation(device, now_ms)
            .inspect_err(|_| {
                self.audit.record(
                    AuditEvent::action(AuditAction::TabCreate, Outcome::Denied).device(device),
                )
            })?;
        let tab = self.adapter.create_tab().await?;
        self.audit
            .record(AuditEvent::action(AuditAction::TabCreate, Outcome::Ok).device(device));
        Ok(tab)
    }

    /// Create a split of `pane` in `tab` under a live runtime-mutation grant.
    pub async fn create_split(
        &self,
        device: &str,
        tab: &str,
        pane: &str,
        direction: SplitDirection,
        now_ms: u64,
    ) -> Result<String, RelayError> {
        if let Err(e) = self.charge_control_rate(device, now_ms) {
            self.audit.record(
                AuditEvent::action(AuditAction::SplitCreate, Outcome::RateLimited)
                    .device(device)
                    .pane(tab, pane),
            );
            return Err(e);
        }
        self.grants
            .lock()
            .expect("grants poisoned")
            .use_runtime_mutation(device, now_ms)
            .inspect_err(|_| {
                self.audit.record(
                    AuditEvent::action(AuditAction::SplitCreate, Outcome::Denied)
                        .device(device)
                        .pane(tab, pane),
                )
            })?;
        let new_pane = self.adapter.create_split(tab, pane, direction).await?;
        self.audit.record(
            AuditEvent::action(AuditAction::SplitCreate, Outcome::Ok)
                .device(device)
                .pane(tab, pane),
        );
        Ok(new_pane)
    }

    /// Structural validation + per-device nonce single-use for a device frame.
    fn validate_device_frame(
        &self,
        device: &str,
        frame: &Value,
        now_ms: u64,
    ) -> Result<(), RelayError> {
        let mut ledgers = self.nonces.lock().expect("nonces poisoned");
        let ledger = ledgers.entry(device.to_string()).or_default();
        validate_frame(frame, now_ms, ledger).map_err(RelayError::Frame)
    }
}

/// The immutable frame identity for one relayed pane, stamped onto every
/// gateway-produced frame so it validates against the terminal-frame schema.
struct FrameContext {
    runtime_id: String,
    session_name: String,
    tab: String,
    pane: String,
    epoch: String,
}

impl FrameContext {
    fn base(&self, kind: &str) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), json!(kind));
        map.insert("protocol_version".into(), json!({ "major": 1, "minor": 0 }));
        map.insert("runtime_id".into(), json!(self.runtime_id));
        map.insert("session_name".into(), json!(self.session_name));
        map.insert("tab_id".into(), json!(self.tab));
        map.insert("pane_id".into(), json!(self.pane));
        map.insert("epoch".into(), json!(self.epoch));
        map
    }

    fn checkpoint_frame(&self, checkpoint: &Checkpoint) -> Value {
        let mut map = self.base("checkpoint");
        map.insert(
            "output_seq".into(),
            json!(checkpoint.output_seq.to_string()),
        );
        map.insert("cols".into(), json!(checkpoint.cols));
        map.insert("rows".into(), json!(checkpoint.rows));
        map.insert(
            "checkpoint".into(),
            json!({
                "ansi_base64": base64::engine::general_purpose::STANDARD.encode(&checkpoint.ansi),
                "byte_count": checkpoint.ansi.len(),
                "state_hash": checkpoint.state_hash,
            }),
        );
        Value::Object(map)
    }

    fn output_frame(&self, chunk: &OutputChunk) -> Value {
        let mut map = self.base("output");
        map.insert("output_seq".into(), json!(chunk.output_seq.to_string()));
        map.insert(
            "payload_base64".into(),
            json!(base64::engine::general_purpose::STANDARD.encode(&chunk.bytes)),
        );
        map.insert("byte_count".into(), json!(chunk.bytes.len()));
        Value::Object(map)
    }

    fn ack_frame(&self, ack_output_seq: u64) -> Value {
        let mut map = self.base("ack");
        map.insert("ack_output_seq".into(), json!(ack_output_seq.to_string()));
        Value::Object(map)
    }

    fn heartbeat_frame(&self, monotonic_ms: u64) -> Value {
        let mut map = self.base("heartbeat");
        map.insert("monotonic_ms".into(), json!(monotonic_ms));
        Value::Object(map)
    }

    fn grant_expiring_frame(&self, scope: &str, expires_at_ms: u64) -> Value {
        let mut map = self.base("grant_expiring");
        map.insert("scope".into(), json!(scope));
        map.insert("expires_at_ms".into(), json!(expires_at_ms));
        Value::Object(map)
    }

    fn control_revoked_frame(&self, scope: &str, reason: &str) -> Value {
        let mut map = self.base("control_revoked");
        map.insert("scope".into(), json!(scope));
        map.insert("reason".into(), json!(reason));
        Value::Object(map)
    }
}

/// A protocol scope's wire spelling.
pub fn scope_wire(scope: crate::grants::Scope) -> &'static str {
    match scope {
        crate::grants::Scope::Observe => "observe",
        crate::grants::Scope::PaneControl => "pane-control",
        crate::grants::Scope::RuntimeMutation => "runtime-mutation",
    }
}

/// Why an active control grant was revoked. The wire values are fixed by the
/// protocol's `control_revoked` reason enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlRevokedReason {
    Backgrounded,
    ConnectionClosed,
    DeviceRevoked,
    GrantExpired,
    ObserveLost,
    PaneChanged,
    RuntimeChanged,
}

impl ControlRevokedReason {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Backgrounded => "backgrounded",
            Self::ConnectionClosed => "connection_closed",
            Self::DeviceRevoked => "device_revoked",
            Self::GrantExpired => "grant_expired",
            Self::ObserveLost => "observe_lost",
            Self::PaneChanged => "pane_changed",
            Self::RuntimeChanged => "runtime_changed",
        }
    }
}

fn context_for(identity: &RuntimeIdentity, tab: &str, pane: &str, epoch: &str) -> FrameContext {
    FrameContext {
        runtime_id: identity.instance_id.clone(),
        session_name: identity.session_name.clone(),
        tab: tab.to_string(),
        pane: pane.to_string(),
        epoch: epoch.to_string(),
    }
}

/// Build an `output` frame for a streamed chunk, stamped with the pane context.
pub fn output_frame(
    identity: &RuntimeIdentity,
    tab: &str,
    pane: &str,
    chunk: &OutputChunk,
) -> Value {
    context_for(identity, tab, pane, &chunk.epoch).output_frame(chunk)
}

/// Build a `heartbeat` frame carrying the connection's monotonic uptime.
pub fn heartbeat_frame(
    identity: &RuntimeIdentity,
    tab: &str,
    pane: &str,
    epoch: &str,
    monotonic_ms: u64,
) -> Value {
    context_for(identity, tab, pane, epoch).heartbeat_frame(monotonic_ms)
}

/// Build a `grant_expiring` frame warning the device a grant is about to lapse.
pub fn grant_expiring_frame(
    identity: &RuntimeIdentity,
    tab: &str,
    pane: &str,
    epoch: &str,
    scope: crate::grants::Scope,
    expires_at_ms: u64,
) -> Value {
    context_for(identity, tab, pane, epoch).grant_expiring_frame(scope_wire(scope), expires_at_ms)
}

/// Build a `control_revoked` frame telling the device a control grant is gone.
pub fn control_revoked_frame(
    identity: &RuntimeIdentity,
    tab: &str,
    pane: &str,
    epoch: &str,
    scope: crate::grants::Scope,
    reason: ControlRevokedReason,
) -> Value {
    context_for(identity, tab, pane, epoch).control_revoked_frame(scope_wire(scope), reason.wire())
}

fn frame_str(frame: &Value, field: &'static str) -> Result<String, RelayError> {
    frame
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(RelayError::Frame(FrameError::MissingField(field)))
}

fn frame_uint(frame: &Value, field: &'static str) -> Result<u64, RelayError> {
    frame
        .get(field)
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(RelayError::Frame(FrameError::InvalidUint64(field)))
}

fn frame_dim(frame: &Value, field: &'static str) -> Result<u16, RelayError> {
    let value = frame
        .get(field)
        .and_then(Value::as_u64)
        .ok_or(RelayError::Frame(FrameError::MissingField(field)))?;
    if !(2..=500).contains(&value) {
        return Err(RelayError::Frame(FrameError::MissingField(field)));
    }
    Ok(value as u16)
}

fn frame_pane(frame: &Value) -> Result<(String, String), RelayError> {
    Ok((frame_str(frame, "tab_id")?, frame_str(frame, "pane_id")?))
}
