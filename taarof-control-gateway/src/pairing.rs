//! Pairing: five-minute one-use offers, challenge-bound enrollment, operator
//! confirmation, and revocation.
//!
//! The owner host asks the gateway to mint a short-lived pairing offer carrying
//! a random challenge. A device enrolls by proving possession of its identity
//! key over that challenge and presenting a hardware attestation for its
//! control key bound to the same challenge (see [`crate::attestation`]). A
//! successful enrollment becomes a *pending* device the operator must confirm
//! over the local channel before it is authorized. Devices can be revoked at
//! any time.
//!
//! This manager is the in-memory decision core. Persisting device records to
//! SQLite and wiring the local confirmation socket are separate concerns.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use ed25519_dalek::{Signature, VerifyingKey};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::attestation::{
    self, wire, AttestationError, AttestationFacts, AttestationPolicy, SecurityLevel,
};
use crate::db;
use crate::error::GatewayError;

/// Pairing offers are valid for five minutes.
pub const OFFER_TTL_MS: u64 = 300_000;

/// A minted pairing offer handed to the owner host to encode in a QR.
///
/// Deliberately does not derive `Debug`: `challenge` should not land in logs.
pub struct PairingOffer {
    pub offer_id: String,
    /// Random challenge the device binds possession and attestation to.
    pub challenge: [u8; 32],
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

struct OfferState {
    challenge: [u8; 32],
    expires_at_ms: u64,
    consumed: bool,
}

/// Everything a device submits to enroll against an offer.
pub struct DeviceEnrollment {
    pub device_name: String,
    /// Identity/observe public key.
    pub observe_public_key: [u8; 32],
    /// Control public key (auth-per-use), vouched for by the attestation chain.
    pub control_public_key: [u8; 32],
    /// Signature over the offer challenge by the observe private key.
    pub possession_signature: [u8; 64],
    /// Hardware attestation chain for the control key, leaf-first.
    pub attestation_chain: Vec<wire::Cert>,
}

/// Enrollment shape for a Linux desktop device. Unlike Android, a generic
/// Linux workstation cannot make the Android hardware-attestation claim. Its
/// software control key is accepted only into the pending queue and still
/// requires explicit owner-host confirmation before authorization.
pub struct DesktopEnrollment {
    pub device_name: String,
    pub observe_public_key: [u8; 32],
    pub control_public_key: [u8; 32],
    pub possession_signature: [u8; 64],
}

struct EnrolledFacts {
    device_name: String,
    observe_key: [u8; 32],
    control_key: [u8; 32],
    key_fingerprint: String,
    attestation: Option<AttestationFacts>,
}

/// A device awaiting operator confirmation.
#[derive(Debug)]
pub struct PendingDevice {
    pub pending_id: String,
    pub device_name: String,
    pub key_fingerprint: String,
}

/// A confirmed, paired device.
#[derive(Debug)]
pub struct Device {
    pub device_id: String,
    pub device_name: String,
    pub key_fingerprint: String,
}

#[derive(Debug)]
pub struct DeviceView {
    pub device_id: String,
    pub device_name: String,
    pub key_fingerprint: String,
    pub revoked: bool,
}

struct PendingRecord {
    pending_id: String,
    facts: EnrolledFacts,
}

/// In-memory authorized-device record. Attestation facts are persisted to the
/// device store, not needed here for authorization decisions.
struct DeviceRecord {
    device_id: String,
    device_name: String,
    observe_key: [u8; 32],
    control_key: [u8; 32],
    revoked: bool,
}

/// Reasons pairing operations fail. Metadata only; no secrets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingError {
    UnknownOffer,
    OfferExpired,
    OfferConsumed,
    BadPossessionProof,
    Attestation(AttestationError),
    UnknownPending,
    UnknownDevice,
    /// The device store could not be read or written.
    Storage(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOffer => f.write_str("no such pairing offer"),
            Self::OfferExpired => f.write_str("pairing offer has expired"),
            Self::OfferConsumed => f.write_str("pairing offer has already been used"),
            Self::BadPossessionProof => f.write_str("device did not prove key possession"),
            Self::Attestation(e) => write!(f, "attestation rejected: {e}"),
            Self::UnknownPending => f.write_str("no such pending device"),
            Self::UnknownDevice => f.write_str("no such device"),
            Self::Storage(e) => write!(f, "device store error: {e}"),
        }
    }
}

impl std::error::Error for PairingError {}

/// Durable storage for confirmed device records. The gateway supplies a
/// SQLite-backed store; pure in-memory use (and unit tests) use the no-op
/// [`MemoryDeviceStore`].
pub trait DeviceStore: Send {
    fn load(&self) -> Result<Vec<db::StoredDevice>, GatewayError>;
    fn insert(&self, device: &db::StoredDevice) -> Result<(), GatewayError>;
    fn mark_revoked(&self, device_uuid: &str, revoked_at_ms: u64) -> Result<(), GatewayError>;
}

/// A store that persists nothing — for callers that do not need durability.
#[derive(Default)]
pub struct MemoryDeviceStore;

impl DeviceStore for MemoryDeviceStore {
    fn load(&self) -> Result<Vec<db::StoredDevice>, GatewayError> {
        Ok(Vec::new())
    }
    fn insert(&self, _device: &db::StoredDevice) -> Result<(), GatewayError> {
        Ok(())
    }
    fn mark_revoked(&self, _device_uuid: &str, _revoked_at_ms: u64) -> Result<(), GatewayError> {
        Ok(())
    }
}

/// SQLite-backed device store using the gateway's `devices` table.
pub struct SqliteDeviceStore {
    conn: Mutex<Connection>,
}

impl SqliteDeviceStore {
    /// Open (and migrate) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, GatewayError> {
        Ok(Self {
            conn: Mutex::new(db::open(path)?),
        })
    }

    /// Wrap an already-open connection (e.g. an in-memory test database).
    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }
}

impl DeviceStore for SqliteDeviceStore {
    fn load(&self) -> Result<Vec<db::StoredDevice>, GatewayError> {
        db::load_devices(&self.conn.lock().expect("device store mutex poisoned"))
    }
    fn insert(&self, device: &db::StoredDevice) -> Result<(), GatewayError> {
        db::insert_device(
            &self.conn.lock().expect("device store mutex poisoned"),
            device,
        )
    }
    fn mark_revoked(&self, device_uuid: &str, revoked_at_ms: u64) -> Result<(), GatewayError> {
        db::set_device_revoked(
            &self.conn.lock().expect("device store mutex poisoned"),
            device_uuid,
            revoked_at_ms,
        )
    }
}

/// Pairing decision core. Confirmed devices are cached in memory and persisted
/// through a [`DeviceStore`] so they survive a gateway restart (spec line 94).
pub struct PairingManager {
    offers: HashMap<String, OfferState>,
    pending: Vec<PendingRecord>,
    devices: Vec<DeviceRecord>,
    store: Box<dyn DeviceStore>,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingManager {
    /// A manager with no durable storage. Confirmed devices live only in memory.
    pub fn new() -> Self {
        Self {
            offers: HashMap::new(),
            pending: Vec::new(),
            devices: Vec::new(),
            store: Box::new(MemoryDeviceStore),
        }
    }

    /// A manager backed by `store`, preloaded with any devices it already holds
    /// (including their revocation state).
    pub fn with_store(store: Box<dyn DeviceStore>) -> Result<Self, PairingError> {
        let devices = store
            .load()
            .map_err(|e| PairingError::Storage(e.to_string()))?
            .into_iter()
            .map(device_record_from_stored)
            .collect();
        Ok(Self {
            offers: HashMap::new(),
            pending: Vec::new(),
            devices,
            store,
        })
    }

    /// Mint a five-minute, one-use offer.
    pub fn create_offer(&mut self, now_ms: u64) -> PairingOffer {
        let offer_id = Uuid::new_v4().to_string();
        let challenge = random_challenge();
        let expires_at_ms = now_ms.saturating_add(OFFER_TTL_MS);
        self.offers.insert(
            offer_id.clone(),
            OfferState {
                challenge,
                expires_at_ms,
                consumed: false,
            },
        );
        PairingOffer {
            offer_id,
            challenge,
            created_at_ms: now_ms,
            expires_at_ms,
        }
    }

    /// Enroll a device against an offer. On success the device is *pending*
    /// operator confirmation; the offer is consumed.
    pub fn submit_enrollment(
        &mut self,
        offer_id: &str,
        now_ms: u64,
        enrollment: DeviceEnrollment,
        policy: &AttestationPolicy,
    ) -> Result<PendingDevice, PairingError> {
        let challenge = {
            let offer = self
                .offers
                .get(offer_id)
                .ok_or(PairingError::UnknownOffer)?;
            if offer.consumed {
                return Err(PairingError::OfferConsumed);
            }
            if now_ms > offer.expires_at_ms {
                return Err(PairingError::OfferExpired);
            }
            offer.challenge
        };

        verify_possession(
            &enrollment.observe_public_key,
            &challenge,
            &enrollment.possession_signature,
        )?;

        let attestation = attestation::verify_control_attestation(
            &enrollment.attestation_chain,
            policy,
            &challenge,
            &enrollment.control_public_key,
        )
        .map_err(PairingError::Attestation)?;

        // Consume the offer only once the device is fully verified, so a bad
        // submission cannot grief a legitimate pairing out of its offer.
        self.offers
            .get_mut(offer_id)
            .expect("offer existed above")
            .consumed = true;

        let pending_id = Uuid::new_v4().to_string();
        let key_fingerprint = fingerprint(&enrollment.observe_public_key);
        self.pending.push(PendingRecord {
            pending_id: pending_id.clone(),
            facts: EnrolledFacts {
                device_name: enrollment.device_name.clone(),
                observe_key: enrollment.observe_public_key,
                control_key: enrollment.control_public_key,
                key_fingerprint: key_fingerprint.clone(),
                attestation: Some(attestation),
            },
        });

        Ok(PendingDevice {
            pending_id,
            device_name: enrollment.device_name,
            key_fingerprint,
        })
    }

    /// Enroll an explicitly identified Linux desktop with a software control
    /// key. This consumes the offer only after the observe-key possession proof
    /// succeeds. Authorization still requires [`Self::confirm`] over the local
    /// owner channel.
    pub fn submit_desktop_enrollment(
        &mut self,
        offer_id: &str,
        now_ms: u64,
        enrollment: DesktopEnrollment,
    ) -> Result<PendingDevice, PairingError> {
        let challenge = {
            let offer = self
                .offers
                .get(offer_id)
                .ok_or(PairingError::UnknownOffer)?;
            if offer.consumed {
                return Err(PairingError::OfferConsumed);
            }
            if now_ms > offer.expires_at_ms {
                return Err(PairingError::OfferExpired);
            }
            offer.challenge
        };
        verify_possession(
            &enrollment.observe_public_key,
            &challenge,
            &enrollment.possession_signature,
        )?;
        self.offers
            .get_mut(offer_id)
            .expect("offer existed above")
            .consumed = true;

        let pending_id = Uuid::new_v4().to_string();
        let key_fingerprint = fingerprint(&enrollment.observe_public_key);
        self.pending.push(PendingRecord {
            pending_id: pending_id.clone(),
            facts: EnrolledFacts {
                device_name: enrollment.device_name.clone(),
                observe_key: enrollment.observe_public_key,
                control_key: enrollment.control_public_key,
                key_fingerprint: key_fingerprint.clone(),
                attestation: None,
            },
        });
        Ok(PendingDevice {
            pending_id,
            device_name: enrollment.device_name,
            key_fingerprint,
        })
    }

    /// Devices awaiting operator confirmation.
    pub fn pending(&self) -> Vec<PendingDevice> {
        self.pending
            .iter()
            .map(|p| PendingDevice {
                pending_id: p.pending_id.clone(),
                device_name: p.facts.device_name.clone(),
                key_fingerprint: p.facts.key_fingerprint.clone(),
            })
            .collect()
    }

    /// Public metadata for owner-host device administration. Key bytes never
    /// leave the manager; only their SHA-256 fingerprint is returned.
    pub fn devices(&self) -> Vec<DeviceView> {
        self.devices
            .iter()
            .map(|device| DeviceView {
                device_id: device.device_id.clone(),
                device_name: device.device_name.clone(),
                key_fingerprint: fingerprint(&device.observe_key),
                revoked: device.revoked,
            })
            .collect()
    }

    /// Confirm a pending device, enrolling it as an authorized device and
    /// persisting it durably before it is reported as confirmed.
    pub fn confirm(&mut self, pending_id: &str, now_ms: u64) -> Result<Device, PairingError> {
        let index = self
            .pending
            .iter()
            .position(|p| p.pending_id == pending_id)
            .ok_or(PairingError::UnknownPending)?;

        let device_id = Uuid::new_v4().to_string();
        let stored = stored_device_from_facts(&device_id, &self.pending[index].facts, now_ms);
        // Persist first: if the store write fails the pending device is left
        // untouched, so the operator can retry rather than lose the enrollment.
        self.store
            .insert(&stored)
            .map_err(|e| PairingError::Storage(e.to_string()))?;

        let record = self.pending.remove(index);
        let device = Device {
            device_id: device_id.clone(),
            device_name: record.facts.device_name.clone(),
            key_fingerprint: record.facts.key_fingerprint.clone(),
        };
        self.devices.push(DeviceRecord {
            device_id,
            device_name: record.facts.device_name,
            observe_key: record.facts.observe_key,
            control_key: record.facts.control_key,
            revoked: false,
        });
        Ok(device)
    }

    /// Reject a pending device, discarding it.
    pub fn reject(&mut self, pending_id: &str) -> Result<(), PairingError> {
        let index = self
            .pending
            .iter()
            .position(|p| p.pending_id == pending_id)
            .ok_or(PairingError::UnknownPending)?;
        self.pending.remove(index);
        Ok(())
    }

    /// Revoke an authorized device, durably then in memory.
    ///
    /// Persist-first, mirroring [`Self::confirm`]: revocation must fail closed.
    /// If the durable write fails we return `Err` and leave the device
    /// authorized in memory — the safe direction, since a restart would also
    /// reload it as authorized, so memory and disk never disagree in the
    /// fail-open direction. The operator sees the error and retries.
    pub fn revoke(&mut self, device_id: &str, now_ms: u64) -> Result<(), PairingError> {
        // Confirm the device exists before any write, so an unknown id surfaces
        // as `UnknownDevice` rather than a storage error.
        if !self.devices.iter().any(|d| d.device_id == device_id) {
            return Err(PairingError::UnknownDevice);
        }
        self.store
            .mark_revoked(device_id, now_ms)
            .map_err(|e| PairingError::Storage(e.to_string()))?;
        if let Some(record) = self.devices.iter_mut().find(|d| d.device_id == device_id) {
            record.revoked = true;
        }
        Ok(())
    }

    /// Count of currently authorized (enrolled, not revoked) devices. Used at
    /// startup to confirm the durable device store was actually loaded.
    pub fn device_count(&self) -> usize {
        self.devices.iter().filter(|d| !d.revoked).count()
    }

    /// Whether a device is currently authorized (enrolled and not revoked).
    pub fn is_authorized(&self, device_id: &str) -> bool {
        self.devices
            .iter()
            .any(|d| d.device_id == device_id && !d.revoked)
    }

    /// Whether any authorized device carries `device_name`. Convenience for the
    /// confirmation UI and tests; production lookups use the device id.
    pub fn is_authorized_name(&self, device_name: &str) -> bool {
        self.devices
            .iter()
            .any(|d| d.device_name == device_name && !d.revoked)
    }

    /// The control public key of an authorized device, if any.
    pub fn device_control_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.devices
            .iter()
            .find(|d| d.device_id == device_id && !d.revoked)
            .map(|d| d.control_key)
    }

    /// The observe public key of an authorized device, if any.
    pub fn device_observe_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.devices
            .iter()
            .find(|d| d.device_id == device_id && !d.revoked)
            .map(|d| d.observe_key)
    }
}

/// Label for a verified security level, stored for audit alongside the device.
fn security_level_label(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::Software => "software",
        SecurityLevel::Tee => "tee",
        SecurityLevel::StrongBox => "strongbox",
    }
}

/// Build the durable record for a confirmed device. Public keys and attestation
/// metadata only — no private material.
fn stored_device_from_facts(
    device_id: &str,
    facts: &EnrolledFacts,
    now_ms: u64,
) -> db::StoredDevice {
    let (attestation_facts, security_level) = match &facts.attestation {
        Some(attestation) => (
            Some(serde_json::json!({ "app_id_hex": hex::encode(&attestation.app_id) }).to_string()),
            Some(security_level_label(attestation.security_level).to_string()),
        ),
        None => (None, Some("software-unattested".to_string())),
    };
    db::StoredDevice {
        device_uuid: device_id.to_string(),
        display_name: facts.device_name.clone(),
        observe_public_key: facts.observe_key.to_vec(),
        control_public_key: facts.control_key.to_vec(),
        attestation_facts,
        security_level,
        created_at_ms: now_ms,
        revoked: false,
    }
}

/// Rebuild an in-memory device record from a persisted one on startup.
fn device_record_from_stored(stored: db::StoredDevice) -> DeviceRecord {
    DeviceRecord {
        device_id: stored.device_uuid,
        device_name: stored.display_name,
        observe_key: to_key_array(&stored.observe_public_key),
        control_key: to_key_array(&stored.control_public_key),
        revoked: stored.revoked,
    }
}

/// Coerce stored key bytes back into a 32-byte array, zero-padding a short or
/// truncating a long value (persisted keys are always exactly 32 bytes).
fn to_key_array(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = bytes.len().min(32);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// Verify the device proved possession of its identity key over the challenge.
fn verify_possession(
    observe_public_key: &[u8; 32],
    challenge: &[u8; 32],
    signature: &[u8; 64],
) -> Result<(), PairingError> {
    let key = VerifyingKey::from_bytes(observe_public_key)
        .map_err(|_| PairingError::BadPossessionProof)?;
    let sig = Signature::from_slice(signature).map_err(|_| PairingError::BadPossessionProof)?;
    key.verify_strict(challenge, &sig)
        .map_err(|_| PairingError::BadPossessionProof)
}

/// `sha256:<hex>` fingerprint of a public key, shown at confirmation time.
fn fingerprint(public_key: &[u8; 32]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(public_key)))
}

fn random_challenge() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("system randomness unavailable");
    bytes
}
