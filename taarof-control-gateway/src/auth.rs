//! Opaque server-side sessions and signed-request verification.
//!
//! Per the design spec, sessions are opaque random identifiers held server-side
//! and bound to a device key, connection epoch, scope, and expiry; they are not
//! self-authorizing bearer tokens. Every REST request signs a canonical string
//! covering method, path, body hash, session id, nonce, and deadline. WebSocket
//! upgrades sign the upgrade target and a one-use nonce. The gateway rejects
//! reused nonces and deadlines more than 30 seconds old.
//!
//! Sessions live only in memory: restarting the gateway invalidates all live
//! grants, exactly as the design requires, while paired-device records persist
//! separately.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

/// Signed messages whose deadline is older than this are rejected.
pub const MAX_DEADLINE_AGE_MS: u64 = 30_000;

/// Signed messages whose deadline is further than this into the future are
/// rejected. Without an upper bound a trusted device could set a far-future
/// deadline and hold a unique nonce in [`NonceCache`] essentially forever
/// (unbounded memory) while extending its own replay window without limit.
pub const MAX_DEADLINE_FUTURE_SKEW_MS: u64 = 30_000;

// Canonical-string format version. Bumped to v2 when the encoding became
// length-prefixed (injective); a v1 signature never validates against v2.
const CANONICAL_REQUEST_CONTEXT: &str = "taarof-signed-request-v2";
const CANONICAL_WS_CONTEXT: &str = "taarof-ws-upgrade-v2";

/// Authorization scope of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Observe,
    PaneControl,
    RuntimeMutation,
}

/// A server-side session record. Never serialized to a client.
struct Session {
    device_uuid: String,
    device_key: [u8; 32],
    scope: Scope,
    epoch: u64,
    expires_at_ms: u64,
}

/// A newly created observe session, returned to the caller for handoff.
pub struct ObserveSession {
    /// Opaque session identifier.
    pub id: String,
    pub expires_at_ms: u64,
}

/// The outcome of verifying a signed request: the authenticated principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub device_uuid: String,
    pub scope: Scope,
    pub epoch: u64,
}

/// Reasons a signed request is rejected. Messages carry no secret material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// The deadline is more than [`MAX_DEADLINE_AGE_MS`] in the past.
    DeadlineStale,
    /// The deadline is more than [`MAX_DEADLINE_FUTURE_SKEW_MS`] in the future.
    DeadlineInFuture,
    /// No live session matches the presented identifier.
    UnknownSession,
    /// The session existed but has expired.
    SessionExpired,
    /// The nonce has already been used.
    NonceReplay,
    /// The signature did not verify under the session's device key.
    BadSignature,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::DeadlineStale => "request deadline is stale",
            Self::DeadlineInFuture => "request deadline is too far in the future",
            Self::UnknownSession => "no such session",
            Self::SessionExpired => "session has expired",
            Self::NonceReplay => "nonce has already been used",
            Self::BadSignature => "request signature did not verify",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for AuthError {}

/// In-memory store of opaque server-side sessions.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a renewable observe session bound to `device_key`.
    pub fn create_observe(
        &self,
        device_uuid: &str,
        device_key: [u8; 32],
        now_ms: u64,
        ttl_ms: u64,
    ) -> ObserveSession {
        self.create(device_uuid, device_key, Scope::Observe, 0, now_ms, ttl_ms)
    }

    /// Create a control session bound to `device_key`, scoped and epoch-pinned.
    pub fn create_control(
        &self,
        device_uuid: &str,
        device_key: [u8; 32],
        scope: Scope,
        epoch: u64,
        now_ms: u64,
        ttl_ms: u64,
    ) -> ObserveSession {
        self.create(device_uuid, device_key, scope, epoch, now_ms, ttl_ms)
    }

    fn create(
        &self,
        device_uuid: &str,
        device_key: [u8; 32],
        scope: Scope,
        epoch: u64,
        now_ms: u64,
        ttl_ms: u64,
    ) -> ObserveSession {
        let id = random_session_id();
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        let session = Session {
            device_uuid: device_uuid.to_string(),
            device_key,
            scope,
            epoch,
            expires_at_ms,
        };
        self.sessions
            .lock()
            .expect("session store poisoned")
            .insert(id.clone(), session);
        ObserveSession { id, expires_at_ms }
    }

    /// Close a session, discarding its grant.
    pub fn close(&self, session_id: &str) {
        self.sessions
            .lock()
            .expect("session store poisoned")
            .remove(session_id);
    }

    /// Close every live session for a revoked device.
    pub fn close_device(&self, device_uuid: &str) {
        self.sessions
            .lock()
            .expect("session store poisoned")
            .retain(|_, session| session.device_uuid != device_uuid);
    }

    /// Drop every session whose expiry has passed. `lookup` already refuses an
    /// expired session, so this is a memory-reclamation sweep rather than a
    /// security boundary: it keeps the store from retaining dead grants
    /// indefinitely. Returns the number of sessions removed.
    pub fn sweep_expired(&self, now_ms: u64) -> usize {
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        let before = sessions.len();
        sessions.retain(|_, session| now_ms <= session.expires_at_ms);
        before - sessions.len()
    }

    /// Look up a live (unexpired) session's authenticating fields.
    fn lookup(&self, session_id: &str, now_ms: u64) -> Result<([u8; 32], Verified), AuthError> {
        let sessions = self.sessions.lock().expect("session store poisoned");
        let session = sessions.get(session_id).ok_or(AuthError::UnknownSession)?;
        if now_ms > session.expires_at_ms {
            return Err(AuthError::SessionExpired);
        }
        Ok((
            session.device_key,
            Verified {
                device_uuid: session.device_uuid.clone(),
                scope: session.scope,
                epoch: session.epoch,
            },
        ))
    }
}

/// Records used nonces so no signed message can be replayed.
#[derive(Default)]
pub struct NonceCache {
    /// Maps a used nonce to the deadline it was presented with, so entries can
    /// be pruned once no fresh replay of them could be accepted anyway.
    used: Mutex<HashMap<String, u64>>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically reject-if-seen and record `nonce`. Returns `true` if the
    /// nonce was fresh (and is now consumed), `false` if it was already used.
    pub fn check_and_consume(&self, nonce: &str, deadline_ms: u64, now_ms: u64) -> bool {
        let mut used = self.used.lock().expect("nonce cache poisoned");
        // Drop nonces whose deadline is already too stale to be re-accepted.
        used.retain(|_, deadline| now_ms <= deadline.saturating_add(MAX_DEADLINE_AGE_MS));
        if used.contains_key(nonce) {
            return false;
        }
        used.insert(nonce.to_string(), deadline_ms);
        true
    }
}

/// Hex-encoded SHA-256 of a request body, used in the canonical string.
pub fn body_sha256_hex(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    hex::encode(digest)
}

/// Build the canonical byte string a REST client signs. The domain-separating
/// context prevents a signature from being reused as a WebSocket upgrade.
pub fn canonical_request_bytes(
    method: &str,
    path: &str,
    body_sha256_hex: &str,
    session_id: &str,
    nonce: &str,
    deadline_ms: u64,
) -> Vec<u8> {
    join_canonical(
        CANONICAL_REQUEST_CONTEXT,
        &[method, path, body_sha256_hex, session_id, nonce],
        deadline_ms,
    )
}

/// Build the canonical byte string a WebSocket upgrade signs.
pub fn canonical_ws_upgrade_bytes(
    target: &str,
    session_id: &str,
    nonce: &str,
    deadline_ms: u64,
) -> Vec<u8> {
    join_canonical(
        CANONICAL_WS_CONTEXT,
        &[target, session_id, nonce],
        deadline_ms,
    )
}

/// Canonical bytes a paired device signs to open an observe session before it
/// has a server-side session id. The proof binds the device id, a fresh client
/// challenge, nonce, and short deadline under a distinct domain separator.
pub fn device_session_proof_bytes(
    device_uuid: &str,
    challenge: &str,
    nonce: &str,
    deadline_ms: u64,
) -> Vec<u8> {
    join_canonical(
        "taarof-device-session-proof-v1",
        &[device_uuid, challenge, nonce],
        deadline_ms,
    )
}

/// Verify a paired device's pre-session proof and atomically consume its nonce.
#[allow(clippy::too_many_arguments)]
pub fn verify_device_session_proof(
    cache: &NonceCache,
    device_key: &[u8; 32],
    device_uuid: &str,
    challenge: &str,
    nonce: &str,
    deadline_ms: u64,
    signature: &[u8],
    now_ms: u64,
) -> Result<(), AuthError> {
    check_deadline_window(deadline_ms, now_ms)?;
    let key = VerifyingKey::from_bytes(device_key).map_err(|_| AuthError::BadSignature)?;
    let signature = Signature::from_slice(signature).map_err(|_| AuthError::BadSignature)?;
    key.verify_strict(
        &device_session_proof_bytes(device_uuid, challenge, nonce, deadline_ms),
        &signature,
    )
    .map_err(|_| AuthError::BadSignature)?;
    if !cache.check_and_consume(nonce, deadline_ms, now_ms) {
        return Err(AuthError::NonceReplay);
    }
    Ok(())
}

/// Length-prefixed, injective framing: each field is written as its 8-byte
/// big-endian byte length followed by its raw bytes. Because every field is
/// self-delimiting, no content — including a delimiter byte or a percent-decoded
/// newline in a path/target — can shift across a field boundary, so two distinct
/// field tuples can never produce identical canonical bytes.
fn join_canonical(context: &str, parts: &[&str], deadline_ms: u64) -> Vec<u8> {
    let mut out = Vec::new();
    push_field(&mut out, context.as_bytes());
    for part in parts {
        push_field(&mut out, part.as_bytes());
    }
    push_field(&mut out, deadline_ms.to_string().as_bytes());
    out
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Verify a signed REST request end to end.
///
/// Order: deadline freshness, session validity, signature, then nonce
/// consumption — so an invalid signature never burns a fresh nonce, and a
/// replay of an accepted request is caught by the nonce check.
#[allow(clippy::too_many_arguments)]
pub fn verify_signed_request(
    store: &SessionStore,
    cache: &NonceCache,
    method: &str,
    path: &str,
    body: &[u8],
    session_id: &str,
    nonce: &str,
    deadline_ms: u64,
    signature: &[u8],
    now_ms: u64,
) -> Result<Verified, AuthError> {
    let canonical = canonical_request_bytes(
        method,
        path,
        &body_sha256_hex(body),
        session_id,
        nonce,
        deadline_ms,
    );
    verify_signed(
        store,
        cache,
        &canonical,
        session_id,
        nonce,
        deadline_ms,
        signature,
        now_ms,
    )
}

/// Verify a signed WebSocket upgrade.
#[allow(clippy::too_many_arguments)]
pub fn verify_ws_upgrade(
    store: &SessionStore,
    cache: &NonceCache,
    target: &str,
    session_id: &str,
    nonce: &str,
    deadline_ms: u64,
    signature: &[u8],
    now_ms: u64,
) -> Result<Verified, AuthError> {
    let canonical = canonical_ws_upgrade_bytes(target, session_id, nonce, deadline_ms);
    verify_signed(
        store,
        cache,
        &canonical,
        session_id,
        nonce,
        deadline_ms,
        signature,
        now_ms,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_signed(
    store: &SessionStore,
    cache: &NonceCache,
    canonical: &[u8],
    session_id: &str,
    nonce: &str,
    deadline_ms: u64,
    signature: &[u8],
    now_ms: u64,
) -> Result<Verified, AuthError> {
    check_deadline_window(deadline_ms, now_ms)?;
    let (device_key, verified) = store.lookup(session_id, now_ms)?;

    let key = VerifyingKey::from_bytes(&device_key).map_err(|_| AuthError::BadSignature)?;
    let sig = Signature::from_slice(signature).map_err(|_| AuthError::BadSignature)?;
    key.verify_strict(canonical, &sig)
        .map_err(|_| AuthError::BadSignature)?;

    if !cache.check_and_consume(nonce, deadline_ms, now_ms) {
        return Err(AuthError::NonceReplay);
    }
    Ok(verified)
}

/// Reject a deadline outside the acceptance window: more than
/// [`MAX_DEADLINE_AGE_MS`] in the past or [`MAX_DEADLINE_FUTURE_SKEW_MS`] in the
/// future. Bounding both sides keeps the replay window tight and caps how long a
/// consumed nonce must be retained.
fn check_deadline_window(deadline_ms: u64, now_ms: u64) -> Result<(), AuthError> {
    if now_ms > deadline_ms.saturating_add(MAX_DEADLINE_AGE_MS) {
        return Err(AuthError::DeadlineStale);
    }
    if deadline_ms > now_ms.saturating_add(MAX_DEADLINE_FUTURE_SKEW_MS) {
        return Err(AuthError::DeadlineInFuture);
    }
    Ok(())
}

/// 256 bits of randomness, base64url without padding: an opaque, unguessable,
/// non-self-describing session identifier.
fn random_session_id() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system randomness unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn ws_upgrade_roundtrip_and_replay() {
        let store = SessionStore::new();
        let cache = NonceCache::new();
        let device = key(7);
        let now = 1_000;
        let session = store.create_observe("d", device.verifying_key().to_bytes(), now, 60_000);

        let nonce = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let deadline = now + 1_000;
        let canonical = canonical_ws_upgrade_bytes("/v1/panes/p1", &session.id, nonce, deadline);
        let sig = device.sign(&canonical).to_bytes();

        assert!(verify_ws_upgrade(
            &store,
            &cache,
            "/v1/panes/p1",
            &session.id,
            nonce,
            deadline,
            &sig,
            now
        )
        .is_ok());
        assert_eq!(
            verify_ws_upgrade(
                &store,
                &cache,
                "/v1/panes/p1",
                &session.id,
                nonce,
                deadline,
                &sig,
                now
            ),
            Err(AuthError::NonceReplay)
        );
    }

    #[test]
    fn expired_session_is_rejected() {
        let store = SessionStore::new();
        let cache = NonceCache::new();
        let device = key(8);
        let session = store.create_observe("d", device.verifying_key().to_bytes(), 0, 100);
        let nonce = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let deadline = 200;
        let canonical = canonical_request_bytes(
            "GET",
            "/v1/runtime",
            &body_sha256_hex(b""),
            &session.id,
            nonce,
            deadline,
        );
        let sig = device.sign(&canonical).to_bytes();
        assert_eq!(
            verify_signed_request(
                &store,
                &cache,
                "GET",
                "/v1/runtime",
                b"",
                &session.id,
                nonce,
                deadline,
                &sig,
                200
            ),
            Err(AuthError::SessionExpired)
        );
    }

    #[test]
    fn unknown_session_is_rejected() {
        let store = SessionStore::new();
        let cache = NonceCache::new();
        let sig = [0u8; 64];
        assert_eq!(
            verify_signed_request(
                &store,
                &cache,
                "GET",
                "/v1/runtime",
                b"",
                "nope",
                "n",
                10_000,
                &sig,
                1
            ),
            Err(AuthError::UnknownSession)
        );
    }

    #[test]
    fn session_ids_are_unique_and_opaque() {
        let store = SessionStore::new();
        let a = store.create_observe("device-uuid-abc", [0; 32], 0, 1);
        let b = store.create_observe("device-uuid-abc", [0; 32], 0, 1);
        assert_ne!(a.id, b.id, "session ids must be unique");
        // Opaque: the id does not embed the device identity or any input.
        assert!(!a.id.contains("device-uuid-abc"));
        assert!(a.id.len() >= 40, "id should carry 256 bits of entropy");
    }

    #[test]
    fn sweep_expired_reclaims_only_dead_sessions() {
        let store = SessionStore::new();
        let live = store.create_observe("d", [0; 32], 1_000, 60_000);
        let dead = store.create_observe("d", [0; 32], 1_000, 100);
        // At t=2_000 the short-lived session has expired; the hour-long one has not.
        assert_eq!(store.sweep_expired(2_000), 1);
        // The live session still authenticates; the swept one is gone.
        assert!(store.lookup(&live.id, 2_000).is_ok());
        assert_eq!(
            store.lookup(&dead.id, 2_000).err(),
            Some(AuthError::UnknownSession)
        );
    }

    #[test]
    fn far_future_deadline_is_rejected() {
        let store = SessionStore::new();
        let cache = NonceCache::new();
        let device = key(9);
        let now = 1_000_000;
        let session = store.create_observe("d", device.verifying_key().to_bytes(), now, 3_600_000);
        let nonce = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        // Deadline well beyond the allowed future skew.
        let deadline = now + MAX_DEADLINE_FUTURE_SKEW_MS + 1_000;
        let canonical = canonical_request_bytes(
            "GET",
            "/v1/runtime",
            &body_sha256_hex(b""),
            &session.id,
            nonce,
            deadline,
        );
        let sig = device.sign(&canonical).to_bytes();
        assert_eq!(
            verify_signed_request(
                &store,
                &cache,
                "GET",
                "/v1/runtime",
                b"",
                &session.id,
                nonce,
                deadline,
                &sig,
                now
            ),
            Err(AuthError::DeadlineInFuture)
        );
    }

    #[test]
    fn canonical_encoding_is_injective_across_field_boundaries() {
        // Under a naive newline join these two tuples collide (both yield
        // "…x\ny\nz…"); the length-prefixed encoding must keep them distinct.
        let hash = body_sha256_hex(b"");
        let a = canonical_request_bytes("x\ny", "z", &hash, "s", "n", 1);
        let b = canonical_request_bytes("x", "y\nz", &hash, "s", "n", 1);
        assert_ne!(a, b, "distinct field tuples must not share canonical bytes");

        // A nonce carrying the old delimiter cannot impersonate another field.
        let c = canonical_request_bytes("GET", "/a", &hash, "sid", "n1", 1);
        let d = canonical_request_bytes("GET", "/a", &hash, "sid\nn1", "", 1);
        assert_ne!(c, d);
    }

    #[test]
    fn nonce_consumed_over_rest_is_rejected_on_ws() {
        // REST and WS share one NonceCache; a nonce spent on either path must
        // not be reusable on the other.
        let store = SessionStore::new();
        let cache = NonceCache::new();
        let device = key(11);
        let now = 1_000;
        let session = store.create_observe("d", device.verifying_key().to_bytes(), now, 60_000);
        let nonce = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let deadline = now + 1_000;

        let rest_canonical = canonical_request_bytes(
            "GET",
            "/v1/runtime",
            &body_sha256_hex(b""),
            &session.id,
            nonce,
            deadline,
        );
        let rest_sig = device.sign(&rest_canonical).to_bytes();
        assert!(verify_signed_request(
            &store,
            &cache,
            "GET",
            "/v1/runtime",
            b"",
            &session.id,
            nonce,
            deadline,
            &rest_sig,
            now
        )
        .is_ok());

        let ws_canonical = canonical_ws_upgrade_bytes("/v1/panes/p1", &session.id, nonce, deadline);
        let ws_sig = device.sign(&ws_canonical).to_bytes();
        assert_eq!(
            verify_ws_upgrade(
                &store,
                &cache,
                "/v1/panes/p1",
                &session.id,
                nonce,
                deadline,
                &ws_sig,
                now
            ),
            Err(AuthError::NonceReplay)
        );
    }
}
