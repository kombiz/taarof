//! Security conformance tests for pairing, attestation, and signed sessions.
//!
//! Every test drives the gateway's real verification code with real Ed25519
//! keys and a real DER-encoded attestation chain — there are no mocks. The
//! synthetic trust anchor and device keys are derived deterministically from
//! fixed seeds so the fixtures are reproducible and committed with the test.

use der::asn1::OctetString;
use der::Encode;
use ed25519_dalek::{Signer, SigningKey};

use taarof_control_gateway::attestation::{
    self, wire, AttestationError, AttestationPolicy, SecurityLevel,
};
use taarof_control_gateway::auth::{self, AuthError, NonceCache, SessionStore};
use taarof_control_gateway::db;
use taarof_control_gateway::error::GatewayError;
use taarof_control_gateway::pairing::{
    DeviceEnrollment, DeviceStore, PairingError, PairingManager, SqliteDeviceStore,
};

// ── Deterministic key material ──────────────────────────────────────────────

/// Pinned attestation root (Google-equivalent trust anchor in production).
const ROOT_SEED: u8 = 0x01;
/// Device identity/observe key.
const OBSERVE_SEED: u8 = 0x02;
/// Device control key (auth-per-use in production).
const CONTROL_SEED: u8 = 0x03;
/// A key that is NOT the pinned trust anchor.
const ROGUE_SEED: u8 = 0x66;

const EXPECTED_APP_ID: &[u8] = b"com.taarof.remote/AA:BB:CC";

fn seeded_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn pub_bytes(key: &SigningKey) -> [u8; 32] {
    key.verifying_key().to_bytes()
}

// ── Attestation-chain fixtures ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn attestation_record(
    challenge: &[u8],
    app_id: &[u8],
    security_level: u8,
    control_key: [u8; 32],
    user_auth_required: bool,
    no_auth_required: bool,
    auth_timeout_secs: u32,
) -> wire::AttestationRecord {
    wire::AttestationRecord {
        challenge: OctetString::new(challenge.to_vec()).unwrap(),
        app_id: OctetString::new(app_id.to_vec()).unwrap(),
        security_level,
        user_auth_required,
        no_auth_required,
        auth_timeout_secs,
        attested_public_key: OctetString::new(control_key.to_vec()).unwrap(),
    }
}

fn cert(
    subject: &[u8],
    subject_public_key: [u8; 32],
    issuer: &[u8],
    issuer_signer: &SigningKey,
    attestation: Option<wire::AttestationRecord>,
) -> wire::Cert {
    let tbs = wire::TbsCert {
        version: 1,
        subject: OctetString::new(subject.to_vec()).unwrap(),
        subject_public_key: OctetString::new(subject_public_key.to_vec()).unwrap(),
        issuer: OctetString::new(issuer.to_vec()).unwrap(),
        attestation,
    };
    let signature = issuer_signer.sign(&tbs.to_der().unwrap());
    wire::Cert {
        tbs,
        signature: OctetString::new(signature.to_bytes().to_vec()).unwrap(),
    }
}

/// A two-cert chain `[leaf, root]`: root self-signed with the trust anchor key,
/// leaf signed by the root and carrying `record`.
fn chain(root_signer: &SigningKey, record: wire::AttestationRecord) -> Vec<wire::Cert> {
    let root_pk = pub_bytes(root_signer);
    let leaf = cert(
        b"leaf",
        pub_bytes(&seeded_key(CONTROL_SEED)),
        b"root",
        root_signer,
        Some(record),
    );
    let root = cert(b"root", root_pk, b"root", root_signer, None);
    vec![leaf, root]
}

fn policy() -> AttestationPolicy {
    AttestationPolicy {
        trust_anchor: pub_bytes(&seeded_key(ROOT_SEED)),
        expected_app_id: EXPECTED_APP_ID.to_vec(),
        min_security_level: SecurityLevel::Tee,
    }
}

/// A record that passes every policy check for `challenge`.
fn good_record(challenge: &[u8]) -> wire::AttestationRecord {
    attestation_record(
        challenge,
        EXPECTED_APP_ID,
        SecurityLevel::StrongBox as u8,
        pub_bytes(&seeded_key(CONTROL_SEED)),
        true,
        false,
        0,
    )
}

// ── 1. Replay ───────────────────────────────────────────────────────────────

#[test]
fn signed_request_nonce_cannot_be_replayed() {
    let store = SessionStore::new();
    let cache = NonceCache::new();
    let device_key = seeded_key(OBSERVE_SEED);
    let now = 1_000_000;

    let session = store.create_observe("device-uuid", pub_bytes(&device_key), now, 3_600_000);

    let nonce = "11111111-1111-4111-8111-111111111111";
    let deadline = now + 10_000;
    let body = br#"{"title":"x"}"#;
    let canonical = auth::canonical_request_bytes(
        "POST",
        "/v1/tabs",
        &auth::body_sha256_hex(body),
        &session.id,
        nonce,
        deadline,
    );
    let signature = device_key.sign(&canonical);

    let first = auth::verify_signed_request(
        &store,
        &cache,
        "POST",
        "/v1/tabs",
        body,
        &session.id,
        nonce,
        deadline,
        &signature.to_bytes(),
        now + 1,
    );
    assert!(first.is_ok(), "first request should verify: {first:?}");

    let replay = auth::verify_signed_request(
        &store,
        &cache,
        "POST",
        "/v1/tabs",
        body,
        &session.id,
        nonce,
        deadline,
        &signature.to_bytes(),
        now + 2,
    );
    assert!(
        matches!(replay, Err(AuthError::NonceReplay)),
        "replayed nonce must be rejected, got {replay:?}"
    );
}

#[test]
fn signed_request_with_stale_deadline_is_rejected() {
    let store = SessionStore::new();
    let cache = NonceCache::new();
    let device_key = seeded_key(OBSERVE_SEED);
    let now = 1_000_000;
    let session = store.create_observe("device-uuid", pub_bytes(&device_key), now, 3_600_000);

    let nonce = "22222222-2222-4222-8222-222222222222";
    let deadline = now; // will be evaluated 31s later
    let body = b"";
    let canonical = auth::canonical_request_bytes(
        "GET",
        "/v1/runtime",
        &auth::body_sha256_hex(body),
        &session.id,
        nonce,
        deadline,
    );
    let signature = device_key.sign(&canonical);

    let result = auth::verify_signed_request(
        &store,
        &cache,
        "GET",
        "/v1/runtime",
        body,
        &session.id,
        nonce,
        deadline,
        &signature.to_bytes(),
        now + 31_000,
    );
    assert!(
        matches!(result, Err(AuthError::DeadlineStale)),
        "deadline older than 30s must be rejected, got {result:?}"
    );
}

#[test]
fn signed_request_with_forged_signature_is_rejected() {
    let store = SessionStore::new();
    let cache = NonceCache::new();
    let device_key = seeded_key(OBSERVE_SEED);
    let rogue = seeded_key(ROGUE_SEED);
    let now = 1_000_000;
    let session = store.create_observe("device-uuid", pub_bytes(&device_key), now, 3_600_000);

    let nonce = "33333333-3333-4333-8333-333333333333";
    let deadline = now + 5_000;
    let body = b"";
    let canonical = auth::canonical_request_bytes(
        "GET",
        "/v1/runtime",
        &auth::body_sha256_hex(body),
        &session.id,
        nonce,
        deadline,
    );
    // Signed by the wrong key.
    let signature = rogue.sign(&canonical);

    let result = auth::verify_signed_request(
        &store,
        &cache,
        "GET",
        "/v1/runtime",
        body,
        &session.id,
        nonce,
        deadline,
        &signature.to_bytes(),
        now + 1,
    );
    assert!(
        matches!(result, Err(AuthError::BadSignature)),
        "signature from a non-session key must be rejected, got {result:?}"
    );
}

// ── Pairing enrollment helper ───────────────────────────────────────────────

fn enrollment(
    offer_challenge: &[u8; 32],
    record: wire::AttestationRecord,
    root_signer: &SigningKey,
) -> DeviceEnrollment {
    let observe = seeded_key(OBSERVE_SEED);
    let possession = observe.sign(offer_challenge);
    DeviceEnrollment {
        device_name: "Pixel".to_string(),
        observe_public_key: pub_bytes(&observe),
        control_public_key: pub_bytes(&seeded_key(CONTROL_SEED)),
        possession_signature: possession.to_bytes(),
        attestation_chain: chain(root_signer, record),
    }
}

// ── 2. Expiry ───────────────────────────────────────────────────────────────

#[test]
fn expired_pairing_offer_is_rejected() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let record = good_record(&offer.challenge);
    let enroll = enrollment(&offer.challenge, record, &seeded_key(ROOT_SEED));

    let too_late = now + 300_000 + 1;
    let result = mgr.submit_enrollment(&offer.offer_id, too_late, enroll, &policy());
    assert!(
        matches!(result, Err(PairingError::OfferExpired)),
        "offer older than five minutes must be rejected, got {result:?}"
    );
}

#[test]
fn pairing_offer_is_single_use() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);

    let first = mgr.submit_enrollment(
        &offer.offer_id,
        now + 1,
        enrollment(
            &offer.challenge,
            good_record(&offer.challenge),
            &seeded_key(ROOT_SEED),
        ),
        &policy(),
    );
    assert!(first.is_ok(), "first enrollment should succeed: {first:?}");

    let second = mgr.submit_enrollment(
        &offer.offer_id,
        now + 2,
        enrollment(
            &offer.challenge,
            good_record(&offer.challenge),
            &seeded_key(ROOT_SEED),
        ),
        &policy(),
    );
    assert!(
        matches!(second, Err(PairingError::OfferConsumed)),
        "a consumed offer must not be reusable, got {second:?}"
    );
}

// ── 3. Rejection ────────────────────────────────────────────────────────────

#[test]
fn operator_rejected_device_is_not_authorized() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let pending = mgr
        .submit_enrollment(
            &offer.offer_id,
            now + 1,
            enrollment(
                &offer.challenge,
                good_record(&offer.challenge),
                &seeded_key(ROOT_SEED),
            ),
            &policy(),
        )
        .expect("valid enrollment becomes pending");

    mgr.reject(&pending.pending_id).expect("reject pending");

    assert!(
        mgr.pending().is_empty(),
        "rejected device must not remain pending"
    );
    assert!(
        !mgr.is_authorized_name("Pixel"),
        "a rejected device must never be authorized"
    );
}

#[test]
fn confirmed_then_revoked_device_loses_authorization() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let pending = mgr
        .submit_enrollment(
            &offer.offer_id,
            now + 1,
            enrollment(
                &offer.challenge,
                good_record(&offer.challenge),
                &seeded_key(ROOT_SEED),
            ),
            &policy(),
        )
        .unwrap();
    let device = mgr
        .confirm(&pending.pending_id, now + 2)
        .expect("confirm device");
    assert!(mgr.is_authorized(&device.device_id));

    mgr.revoke(&device.device_id, now + 3)
        .expect("revoke device");
    assert!(
        !mgr.is_authorized(&device.device_id),
        "revoked device must lose authorization"
    );
}

// ── 4. Bad chain ────────────────────────────────────────────────────────────

#[test]
fn attestation_with_untrusted_chain_is_rejected() {
    // Leaf is signed by a rogue key whose self-signed root is NOT the anchor.
    let rogue_root = seeded_key(ROGUE_SEED);
    let challenge = [0x11; 32];
    let bad = chain(&rogue_root, good_record(&challenge));

    let result = attestation::verify_control_attestation(
        &bad,
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(
            result,
            Err(AttestationError::UntrustedAnchor) | Err(AttestationError::BadChain)
        ),
        "a chain not rooted in the pinned anchor must be rejected, got {result:?}"
    );
}

#[test]
fn attestation_with_broken_signature_is_rejected() {
    let root = seeded_key(ROOT_SEED);
    let challenge = [0x11; 32];
    let mut bad = chain(&root, good_record(&challenge));
    // Corrupt the leaf signature.
    let mut sig = bad[0].signature.as_bytes().to_vec();
    sig[0] ^= 0xff;
    bad[0].signature = OctetString::new(sig).unwrap();

    let result = attestation::verify_control_attestation(
        &bad,
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::BadChain)),
        "a tampered signature must be rejected, got {result:?}"
    );
}

// ── 5. Wrong app ────────────────────────────────────────────────────────────

#[test]
fn attestation_with_wrong_application_is_rejected() {
    let root = seeded_key(ROOT_SEED);
    let challenge = [0x22; 32];
    let record = attestation_record(
        &challenge,
        b"com.malware.app",
        SecurityLevel::StrongBox as u8,
        pub_bytes(&seeded_key(CONTROL_SEED)),
        true,
        false,
        0,
    );
    let result = attestation::verify_control_attestation(
        &chain(&root, record),
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::WrongApp)),
        "attestation from the wrong application must be rejected, got {result:?}"
    );
}

// ── 6. Wrong challenge ──────────────────────────────────────────────────────

#[test]
fn attestation_bound_to_wrong_challenge_is_rejected() {
    let root = seeded_key(ROOT_SEED);
    // Same length as the expected challenge but different content, so the
    // constant-time content comparison — not the length check — does the reject.
    let bound = [0x33u8; 32];
    let expected = [0x34u8; 32];
    assert_eq!(bound.len(), expected.len());
    let record = good_record(&bound);
    let result = attestation::verify_control_attestation(
        &chain(&root, record),
        &policy(),
        &expected,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::WrongChallenge)),
        "attestation not bound to the server challenge must be rejected, got {result:?}"
    );
}

// ── 7. Unrestricted control key ─────────────────────────────────────────────

#[test]
fn attestation_for_unrestricted_control_key_is_rejected() {
    let root = seeded_key(ROOT_SEED);
    let challenge = [0x44; 32];
    // no_auth_required = true: the key can sign without user verification.
    let record = attestation_record(
        &challenge,
        EXPECTED_APP_ID,
        SecurityLevel::StrongBox as u8,
        pub_bytes(&seeded_key(CONTROL_SEED)),
        false,
        true,
        0,
    );
    let result = attestation::verify_control_attestation(
        &chain(&root, record),
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::UnrestrictedControlKey)),
        "a control key not requiring per-use auth must be rejected, got {result:?}"
    );
}

#[test]
fn attestation_with_timed_auth_is_rejected_as_unrestricted() {
    let root = seeded_key(ROOT_SEED);
    let challenge = [0x55; 32];
    // Time-based auth (auth_timeout_secs > 0) is not auth-per-use.
    let record = attestation_record(
        &challenge,
        EXPECTED_APP_ID,
        SecurityLevel::StrongBox as u8,
        pub_bytes(&seeded_key(CONTROL_SEED)),
        true,
        false,
        300,
    );
    let result = attestation::verify_control_attestation(
        &chain(&root, record),
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::UnrestrictedControlKey)),
        "a time-authorised (not per-use) control key must be rejected, got {result:?}"
    );
}

#[test]
fn attestation_below_min_security_level_is_rejected() {
    let root = seeded_key(ROOT_SEED);
    let challenge = [0x77; 32];
    let record = attestation_record(
        &challenge,
        EXPECTED_APP_ID,
        SecurityLevel::Software as u8,
        pub_bytes(&seeded_key(CONTROL_SEED)),
        true,
        false,
        0,
    );
    let result = attestation::verify_control_attestation(
        &chain(&root, record),
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert!(
        matches!(result, Err(AttestationError::WeakSecurityLevel)),
        "software-only attestation must be rejected, got {result:?}"
    );
}

// ── Positive path: a fully valid enrollment ─────────────────────────────────

#[test]
fn valid_enrollment_confirms_and_authorizes() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let pending = mgr
        .submit_enrollment(
            &offer.offer_id,
            now + 1,
            enrollment(
                &offer.challenge,
                good_record(&offer.challenge),
                &seeded_key(ROOT_SEED),
            ),
            &policy(),
        )
        .expect("valid enrollment");
    assert_eq!(pending.device_name, "Pixel");

    let device = mgr.confirm(&pending.pending_id, now + 2).expect("confirm");
    assert!(mgr.is_authorized(&device.device_id));
    assert_eq!(
        mgr.device_control_key(&device.device_id),
        Some(pub_bytes(&seeded_key(CONTROL_SEED)))
    );
}

#[test]
fn enrollment_with_bad_possession_proof_is_rejected() {
    let mut mgr = PairingManager::new();
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let mut enroll = enrollment(
        &offer.challenge,
        good_record(&offer.challenge),
        &seeded_key(ROOT_SEED),
    );
    // Corrupt the possession signature.
    enroll.possession_signature[0] ^= 0xff;

    let result = mgr.submit_enrollment(&offer.offer_id, now + 1, enroll, &policy());
    assert!(
        matches!(result, Err(PairingError::BadPossessionProof)),
        "enrollment without proof of key possession must be rejected, got {result:?}"
    );
}

// ── Device persistence across gateway restart (design spec line 94) ─────────

fn temp_db_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "taarof-gw-devices-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("gateway.db")
}

/// Confirm a device against a store at `db_path`, returning its id.
fn confirm_device_in(db_path: &std::path::Path) -> String {
    let store = SqliteDeviceStore::open(db_path).expect("open device store");
    let mut mgr = PairingManager::with_store(Box::new(store)).expect("load store");
    let now = 5_000_000;
    let offer = mgr.create_offer(now);
    let pending = mgr
        .submit_enrollment(
            &offer.offer_id,
            now + 1,
            enrollment(
                &offer.challenge,
                good_record(&offer.challenge),
                &seeded_key(ROOT_SEED),
            ),
            &policy(),
        )
        .expect("valid enrollment");
    mgr.confirm(&pending.pending_id, now + 2)
        .expect("confirm device")
        .device_id
}

#[test]
fn confirmed_device_survives_restart() {
    let db_path = temp_db_path();
    let device_id = confirm_device_in(&db_path);

    // Reopen with a fresh manager and store: the device must still be authorized.
    let store = SqliteDeviceStore::open(&db_path).expect("reopen device store");
    let mgr = PairingManager::with_store(Box::new(store)).expect("reload store");
    assert!(
        mgr.is_authorized(&device_id),
        "confirmed device must persist across a gateway restart"
    );
    assert_eq!(
        mgr.device_control_key(&device_id),
        Some(pub_bytes(&seeded_key(CONTROL_SEED))),
        "control key must be restored from storage"
    );
}

#[test]
fn revoked_device_stays_revoked_after_restart() {
    let db_path = temp_db_path();
    let device_id = confirm_device_in(&db_path);

    {
        let store = SqliteDeviceStore::open(&db_path).expect("reopen device store");
        let mut mgr = PairingManager::with_store(Box::new(store)).expect("reload store");
        mgr.revoke(&device_id, 6_000_000).expect("revoke device");
    }

    // A later reload must still see the revocation.
    let store = SqliteDeviceStore::open(&db_path).expect("reopen device store again");
    let mgr = PairingManager::with_store(Box::new(store)).expect("reload store again");
    assert!(
        !mgr.is_authorized(&device_id),
        "revoked device must stay revoked across a gateway restart"
    );
}

/// A store that loads one device but always fails to persist a revocation.
struct FailingRevokeStore {
    device: db::StoredDevice,
}

impl DeviceStore for FailingRevokeStore {
    fn load(&self) -> Result<Vec<db::StoredDevice>, GatewayError> {
        Ok(vec![self.device.clone()])
    }
    fn insert(&self, _device: &db::StoredDevice) -> Result<(), GatewayError> {
        Ok(())
    }
    fn mark_revoked(&self, _device_uuid: &str, _revoked_at_ms: u64) -> Result<(), GatewayError> {
        Err(GatewayError::Io(std::io::Error::other(
            "simulated device-store failure",
        )))
    }
}

#[test]
fn revoke_that_fails_to_persist_does_not_deauthorize() {
    let device = db::StoredDevice {
        device_uuid: "dev-1".to_string(),
        display_name: "Pixel".to_string(),
        observe_public_key: pub_bytes(&seeded_key(OBSERVE_SEED)).to_vec(),
        control_public_key: pub_bytes(&seeded_key(CONTROL_SEED)).to_vec(),
        attestation_facts: None,
        security_level: Some("strongbox".to_string()),
        created_at_ms: 1,
        revoked: false,
    };
    let mut mgr =
        PairingManager::with_store(Box::new(FailingRevokeStore { device })).expect("load store");
    assert!(mgr.is_authorized("dev-1"));

    let result = mgr.revoke("dev-1", 2);
    assert!(
        matches!(result, Err(PairingError::Storage(_))),
        "a failed durable write must surface as an error, got {result:?}"
    );
    // Fail-closed: because the durable write failed, in-memory state must NOT
    // flip to revoked — otherwise a restart (which reloads from disk) would
    // resurrect the device as authorized. Memory and disk stay in agreement.
    assert!(
        mgr.is_authorized("dev-1"),
        "a revoke whose store write failed must not silently deauthorize in memory only"
    );
}

// ── Attestation chain bound ──────────────────────────────────────────────────

#[test]
fn over_long_attestation_chain_is_rejected_before_verification() {
    let root_signer = seeded_key(ROOT_SEED);
    let challenge = [0x11u8; 32];
    // A well-formed two-cert chain padded past the maximum with clones. The
    // length guard must reject it before doing per-certificate signature work.
    let mut padded = chain(&root_signer, good_record(&challenge));
    let filler = padded[0].clone();
    while padded.len() <= attestation::MAX_ATTESTATION_CHAIN_LEN {
        padded.push(filler.clone());
    }
    let result = attestation::verify_control_attestation(
        &padded,
        &policy(),
        &challenge,
        &pub_bytes(&seeded_key(CONTROL_SEED)),
    );
    assert_eq!(result, Err(AttestationError::BadChain));
}
