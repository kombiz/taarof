//! Public HTTP surface for the remote-control gateway.
//!
//! The TCP router contains only remotely safe operations. Owner-host pairing
//! administration will be served on a separate mode-0600 Unix socket; it must
//! never be mounted here because Caddy can reach every TCP route on this
//! listener.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, NonceCache, Scope, SessionStore, Verified};
use crate::pairing::{DesktopEnrollment, PairingError, PairingManager};
use crate::runtime::RuntimeIdentity;

const HEADER_SESSION: &str = "taarof-session";
const HEADER_NONCE: &str = "taarof-nonce";
const HEADER_DEADLINE: &str = "taarof-deadline";
const HEADER_SIGNATURE: &str = "taarof-signature";

/// Shared, restart-scoped gateway state. Session and nonce stores intentionally
/// remain in memory so a gateway restart invalidates every live grant.
pub struct GatewayServerState {
    identity: RuntimeIdentity,
    pub sessions: SessionStore,
    nonces: NonceCache,
    pairing: Mutex<PairingManager>,
}

impl GatewayServerState {
    pub fn new(identity: RuntimeIdentity) -> Self {
        Self {
            identity,
            sessions: SessionStore::new(),
            nonces: NonceCache::new(),
            pairing: Mutex::new(PairingManager::new()),
        }
    }

    pub fn with_pairing(identity: RuntimeIdentity, pairing: PairingManager) -> Self {
        Self {
            identity,
            sessions: SessionStore::new(),
            nonces: NonceCache::new(),
            pairing: Mutex::new(pairing),
        }
    }

    pub fn identity(&self) -> &RuntimeIdentity {
        &self.identity
    }
}

/// Build the Caddy-facing router. No owner-host administration belongs here.
pub fn public_router(state: Arc<GatewayServerState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/negotiate", post(negotiate))
        .route("/v1/runtime", get(runtime_state))
        .route("/v1/observe-sessions", post(create_observe_session))
        .route("/v1/pairing/offers/{offer_id}/enroll", post(enroll_desktop))
        .with_state(state)
}

/// Owner-host administration router. Serve this only on the gateway's private
/// Unix socket; never merge it into [`public_router`].
pub fn owner_router(state: Arc<GatewayServerState>) -> Router {
    Router::new()
        .route("/v1/pairing/offers", post(owner_create_offer))
        .route("/v1/pairing/pending", get(owner_pending))
        .route(
            "/v1/pairing/pending/{pending_id}/confirm",
            post(owner_confirm),
        )
        .route(
            "/v1/pairing/pending/{pending_id}/reject",
            post(owner_reject),
        )
        .route("/v1/devices", get(owner_devices))
        .route(
            "/v1/devices/{device_id}",
            axum::routing::delete(owner_revoke),
        )
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolVersion {
    major: u16,
    minor: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NegotiateRequest {
    protocol_version: ProtocolVersion,
    capabilities: Vec<String>,
    nonce: String,
    deadline_ms: u64,
}

#[derive(Debug, Serialize)]
struct NegotiateResponse {
    protocol_version: ProtocolVersionResponse,
    capabilities: Vec<&'static str>,
    limits: ResourceLimits,
}

#[derive(Debug, Serialize)]
struct ProtocolVersionResponse {
    major: u16,
    minor: u16,
}

#[derive(Debug, Serialize)]
struct ResourceLimits {
    max_attached_panes: usize,
    max_control_frame_bytes: u64,
    max_output_frame_bytes: u64,
    metadata_requests_per_minute: u32,
    control_operations_per_second: u32,
}

async fn negotiate(Json(request): Json<NegotiateRequest>) -> Response {
    let now = now_ms();
    if request.protocol_version.major != 1
        || request.protocol_version.minor > 0
        || uuid::Uuid::parse_str(&request.nonce).is_err()
        || request.deadline_ms < now.saturating_sub(auth::MAX_DEADLINE_AGE_MS)
        || request.deadline_ms > now.saturating_add(auth::MAX_DEADLINE_FUTURE_SKEW_MS)
    {
        return api_error(StatusCode::BAD_REQUEST, "invalid_negotiation");
    }

    let supported = ["raw_pty", "runtime_mutation", "biometric_user_verification"];
    let capabilities = supported
        .into_iter()
        .filter(|candidate| request.capabilities.iter().any(|value| value == candidate))
        .collect();
    Json(NegotiateResponse {
        protocol_version: ProtocolVersionResponse { major: 1, minor: 0 },
        capabilities,
        limits: ResourceLimits {
            max_attached_panes: crate::grants::MAX_ATTACHED_PANES,
            max_control_frame_bytes: crate::terminal::CONTROL_FRAME_MAX_BYTES,
            max_output_frame_bytes: crate::terminal::OUTPUT_FRAME_MAX_BYTES,
            metadata_requests_per_minute: crate::grants::METADATA_REQUESTS_PER_MINUTE,
            control_operations_per_second: crate::grants::CONTROL_OPERATIONS_PER_SECOND,
        },
    })
    .into_response()
}

async fn runtime_state(State(state): State<Arc<GatewayServerState>>, request: Request) -> Response {
    match authenticate(&state, &request, &[], &[Scope::Observe]) {
        Ok(_) => Json(json!({
            "runtime": {
                "instance_id": state.identity.instance_id,
                "session_name": state.identity.session_name,
            },
            "status": "online",
            "capabilities": ["raw_pty", "runtime_mutation"],
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveSessionRequest {
    device_id: String,
    server_challenge: String,
    signature: String,
    nonce: String,
    deadline_ms: u64,
}

async fn create_observe_session(
    State(state): State<Arc<GatewayServerState>>,
    Json(body): Json<ObserveSessionRequest>,
) -> Response {
    if uuid::Uuid::parse_str(&body.device_id).is_err()
        || uuid::Uuid::parse_str(&body.nonce).is_err()
        || body.server_challenge.len() < 32
        || body.server_challenge.len() > 512
    {
        return api_error(StatusCode::BAD_REQUEST, "invalid_session_request");
    }
    let key = match state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .device_observe_key(&body.device_id)
    {
        Some(key) => key,
        None => return api_error(StatusCode::UNAUTHORIZED, "invalid_authentication"),
    };
    let signature = match base64::engine::general_purpose::STANDARD.decode(&body.signature) {
        Ok(value) => value,
        Err(_) => return api_error(StatusCode::UNAUTHORIZED, "invalid_authentication"),
    };
    let now = now_ms();
    if auth::verify_device_session_proof(
        &state.nonces,
        &key,
        &body.device_id,
        &body.server_challenge,
        &body.nonce,
        body.deadline_ms,
        &signature,
        now,
    )
    .is_err()
    {
        return api_error(StatusCode::UNAUTHORIZED, "invalid_authentication");
    }
    let session =
        state
            .sessions
            .create_observe(&body.device_id, key, now, crate::grants::OBSERVE_TTL_MS);
    (
        StatusCode::CREATED,
        Json(json!({
            "observe_session_id": session.id,
            "scope": "observe",
            "expires_at_ms": session.expires_at_ms,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopEnrollmentRequest {
    device_class: String,
    device_name: String,
    observe_public_key: String,
    control_public_key: String,
    possession_signature: String,
}

async fn enroll_desktop(
    State(state): State<Arc<GatewayServerState>>,
    Path(offer_id): Path<String>,
    Json(body): Json<DesktopEnrollmentRequest>,
) -> Response {
    if body.device_class != "linux-desktop"
        || body.device_name.is_empty()
        || body.device_name.len() > 128
    {
        return api_error(StatusCode::BAD_REQUEST, "invalid_enrollment");
    }
    let enrollment = match decode_desktop_enrollment(body) {
        Ok(value) => value,
        Err(()) => return api_error(StatusCode::BAD_REQUEST, "invalid_enrollment"),
    };
    let result = state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .submit_desktop_enrollment(&offer_id, now_ms(), enrollment);
    match result {
        Ok(pending) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "pending_id": pending.pending_id,
                "device_name": pending.device_name,
                "key_fingerprint": pending.key_fingerprint,
            })),
        )
            .into_response(),
        Err(error) => pairing_error(error),
    }
}

fn decode_desktop_enrollment(body: DesktopEnrollmentRequest) -> Result<DesktopEnrollment, ()> {
    let observe = decode_array::<32>(&body.observe_public_key)?;
    let control = decode_array::<32>(&body.control_public_key)?;
    let signature = decode_array::<64>(&body.possession_signature)?;
    Ok(DesktopEnrollment {
        device_name: body.device_name,
        observe_public_key: observe,
        control_public_key: control,
        possession_signature: signature,
    })
}

fn decode_array<const N: usize>(encoded: &str) -> Result<[u8; N], ()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ())?;
    bytes.try_into().map_err(|_| ())
}

async fn owner_create_offer(State(state): State<Arc<GatewayServerState>>) -> Response {
    let offer = state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .create_offer(now_ms());
    Json(json!({
        "offer_id": offer.offer_id,
        "challenge_base64": base64::engine::general_purpose::STANDARD.encode(offer.challenge),
        "expires_at_ms": offer.expires_at_ms,
        "protocol_version": { "major": 1, "minor": 0 },
    }))
    .into_response()
}

async fn owner_pending(State(state): State<Arc<GatewayServerState>>) -> Response {
    let pending = state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .pending()
        .into_iter()
        .map(|item| {
            json!({
                "pending_id": item.pending_id,
                "device_name": item.device_name,
                "key_fingerprint": item.key_fingerprint,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "pending": pending })).into_response()
}

async fn owner_confirm(
    State(state): State<Arc<GatewayServerState>>,
    Path(pending_id): Path<String>,
) -> Response {
    match state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .confirm(&pending_id, now_ms())
    {
        Ok(device) => Json(json!({
            "device_id": device.device_id,
            "device_name": device.device_name,
            "key_fingerprint": device.key_fingerprint,
        }))
        .into_response(),
        Err(error) => pairing_error(error),
    }
}

async fn owner_reject(
    State(state): State<Arc<GatewayServerState>>,
    Path(pending_id): Path<String>,
) -> Response {
    match state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .reject(&pending_id)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => pairing_error(error),
    }
}

async fn owner_devices(State(state): State<Arc<GatewayServerState>>) -> Response {
    let devices = state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .devices()
        .into_iter()
        .map(|item| {
            json!({
                "device_id": item.device_id,
                "device_name": item.device_name,
                "key_fingerprint": item.key_fingerprint,
                "revoked": item.revoked,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "devices": devices })).into_response()
}

async fn owner_revoke(
    State(state): State<Arc<GatewayServerState>>,
    Path(device_id): Path<String>,
) -> Response {
    match state
        .pairing
        .lock()
        .expect("pairing manager poisoned")
        .revoke(&device_id, now_ms())
    {
        Ok(()) => {
            state.sessions.close_device(&device_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => pairing_error(error),
    }
}

fn pairing_error(error: PairingError) -> Response {
    let status = match error {
        PairingError::UnknownOffer | PairingError::UnknownPending | PairingError::UnknownDevice => {
            StatusCode::NOT_FOUND
        }
        PairingError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    api_error(status, "pairing_rejected")
}

#[derive(Clone, Copy)]
enum ApiAuthError {
    Unauthorized,
    Forbidden,
}

impl ApiAuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => api_error(StatusCode::UNAUTHORIZED, "invalid_authentication"),
            Self::Forbidden => api_error(StatusCode::FORBIDDEN, "insufficient_scope"),
        }
    }
}

fn authenticate(
    state: &GatewayServerState,
    request: &Request,
    body: &[u8],
    allowed_scopes: &[Scope],
) -> Result<Verified, ApiAuthError> {
    let headers = request.headers();
    let session = required_header(headers, HEADER_SESSION).ok_or(ApiAuthError::Unauthorized)?;
    let nonce = required_header(headers, HEADER_NONCE).ok_or(ApiAuthError::Unauthorized)?;
    let deadline = required_header(headers, HEADER_DEADLINE)
        .ok_or(ApiAuthError::Unauthorized)?
        .parse::<u64>()
        .map_err(|_| ApiAuthError::Unauthorized)?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(required_header(headers, HEADER_SIGNATURE).ok_or(ApiAuthError::Unauthorized)?)
        .map_err(|_| ApiAuthError::Unauthorized)?;
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(request.uri().path());
    let verified = auth::verify_signed_request(
        &state.sessions,
        &state.nonces,
        request.method().as_str(),
        path,
        body,
        session,
        nonce,
        deadline,
        &signature,
        now_ms(),
    )
    .map_err(|_| ApiAuthError::Unauthorized)?;
    if !allowed_scopes.contains(&verified.scope) {
        return Err(ApiAuthError::Forbidden);
    }
    Ok(verified)
}

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

fn api_error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use ed25519_dalek::{Signer, SigningKey};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    #[tokio::test]
    async fn runtime_requires_a_valid_signed_observe_session() {
        let state = Arc::new(GatewayServerState::new(RuntimeIdentity::new(
            "runtime-1",
            "default",
        )));
        let unsigned = public_router(state.clone())
            .oneshot(HttpRequest::get("/v1/runtime").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);

        let key = SigningKey::from_bytes(&[7_u8; 32]);
        let now = now_ms();
        let session =
            state
                .sessions
                .create_observe("device-1", key.verifying_key().to_bytes(), now, 60_000);
        let nonce = uuid::Uuid::new_v4().to_string();
        let deadline = now + 15_000;
        let canonical = auth::canonical_request_bytes(
            "GET",
            "/v1/runtime",
            &auth::body_sha256_hex(&[]),
            &session.id,
            &nonce,
            deadline,
        );
        let signature =
            base64::engine::general_purpose::STANDARD.encode(key.sign(&canonical).to_bytes());
        let response = public_router(state)
            .oneshot(
                HttpRequest::get("/v1/runtime")
                    .header(HEADER_SESSION, session.id)
                    .header(HEADER_NONCE, nonce)
                    .header(HEADER_DEADLINE, deadline.to_string())
                    .header(HEADER_SIGNATURE, signature)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["runtime"]["instance_id"], "runtime-1");
    }

    #[tokio::test]
    async fn negotiation_rejects_unknown_major_and_returns_limits() {
        let state = Arc::new(GatewayServerState::new(RuntimeIdentity::new(
            "runtime-1",
            "default",
        )));
        let now = now_ms();
        let body = json!({
            "protocol_version": { "major": 1, "minor": 0 },
            "capabilities": ["raw_pty", "runtime_mutation", "unknown"],
            "nonce": uuid::Uuid::new_v4().to_string(),
            "deadline_ms": now + 15_000,
        });
        let response = public_router(state)
            .oneshot(
                HttpRequest::post("/v1/negotiate")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["capabilities"],
            json!(["raw_pty", "runtime_mutation"])
        );
        assert_eq!(value["limits"]["max_attached_panes"], 4);
    }

    #[tokio::test]
    async fn desktop_pairing_requires_owner_socket_confirmation() {
        let state = Arc::new(GatewayServerState::new(RuntimeIdentity::new(
            "runtime-1",
            "default",
        )));
        let public = public_router(state.clone());
        let owner = owner_router(state.clone());

        let hidden = public
            .clone()
            .oneshot(
                HttpRequest::post("/v1/pairing/offers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        let offer_response = owner
            .clone()
            .oneshot(
                HttpRequest::post("/v1/pairing/offers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = offer_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let offer: Value = serde_json::from_slice(&bytes).unwrap();
        let challenge = base64::engine::general_purpose::STANDARD
            .decode(offer["challenge_base64"].as_str().unwrap())
            .unwrap();
        let identity_key = SigningKey::from_bytes(&[9_u8; 32]);
        let control_key = SigningKey::from_bytes(&[10_u8; 32]);
        let enrollment = json!({
            "device_class": "linux-desktop",
            "device_name": "omarchy",
            "observe_public_key": base64::engine::general_purpose::STANDARD.encode(identity_key.verifying_key().to_bytes()),
            "control_public_key": base64::engine::general_purpose::STANDARD.encode(control_key.verifying_key().to_bytes()),
            "possession_signature": base64::engine::general_purpose::STANDARD.encode(identity_key.sign(&challenge).to_bytes()),
        });
        let enrolled = public
            .oneshot(
                HttpRequest::post(format!(
                    "/v1/pairing/offers/{}/enroll",
                    offer["offer_id"].as_str().unwrap()
                ))
                .header("content-type", "application/json")
                .body(Body::from(enrollment.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(enrolled.status(), StatusCode::ACCEPTED);
        assert_eq!(state.pairing.lock().unwrap().device_count(), 0);

        let pending_response = owner
            .clone()
            .oneshot(
                HttpRequest::get("/v1/pairing/pending")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = pending_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let pending: Value = serde_json::from_slice(&bytes).unwrap();
        let pending_id = pending["pending"][0]["pending_id"].as_str().unwrap();
        let confirmed = owner
            .oneshot(
                HttpRequest::post(format!("/v1/pairing/pending/{pending_id}/confirm"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(confirmed.status(), StatusCode::OK);
        let bytes = confirmed.into_body().collect().await.unwrap().to_bytes();
        let confirmed: Value = serde_json::from_slice(&bytes).unwrap();
        let device_id = confirmed["device_id"].as_str().unwrap();
        assert_eq!(state.pairing.lock().unwrap().device_count(), 1);

        let now = now_ms();
        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = uuid::Uuid::new_v4().to_string();
        let deadline = now + 15_000;
        let proof = auth::device_session_proof_bytes(device_id, &challenge, &nonce, deadline);
        let body = json!({
            "device_id": device_id,
            "server_challenge": challenge,
            "signature": base64::engine::general_purpose::STANDARD.encode(identity_key.sign(&proof).to_bytes()),
            "nonce": nonce,
            "deadline_ms": deadline,
        });
        let session_response = public_router(state.clone())
            .oneshot(
                HttpRequest::post("/v1/observe-sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session_response.status(), StatusCode::CREATED);
        let bytes = session_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let session: Value = serde_json::from_slice(&bytes).unwrap();
        let session_id = session["observe_session_id"].as_str().unwrap();

        let request_nonce = uuid::Uuid::new_v4().to_string();
        let request_deadline = now + 20_000;
        let canonical = auth::canonical_request_bytes(
            "GET",
            "/v1/runtime",
            &auth::body_sha256_hex(&[]),
            session_id,
            &request_nonce,
            request_deadline,
        );
        let runtime_response = public_router(state)
            .oneshot(
                HttpRequest::get("/v1/runtime")
                    .header(HEADER_SESSION, session_id)
                    .header(HEADER_NONCE, request_nonce)
                    .header(HEADER_DEADLINE, request_deadline.to_string())
                    .header(
                        HEADER_SIGNATURE,
                        base64::engine::general_purpose::STANDARD
                            .encode(identity_key.sign(&canonical).to_bytes()),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(runtime_response.status(), StatusCode::OK);
    }
}
