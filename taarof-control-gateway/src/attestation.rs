//! Hardware key-attestation policy for enrolling remote control keys.
//!
//! A remote device may only receive a control grant if its control key is
//! backed by acceptable hardware attestation. The gateway verifies, per the
//! design spec's pairing section:
//!
//! - the attestation **chain** signs up to a pinned trust anchor;
//! - the attested **application identity** matches the expected client;
//! - the attestation is bound to the server-issued **challenge**;
//! - the key lives at a TEE/StrongBox **security level**; and
//! - the key's **authorization list** proves auth-per-use (biometric or device
//!   credential), i.e. it is not an unrestricted signing key.
//!
//! Android's real attestation is an X.509 chain whose leaf carries a DER
//! `KeyDescription` extension. To keep this testable headless without Android
//! hardware, the wire form here is a minimal DER certificate signed with
//! Ed25519; the verification logic — chain-of-trust to a pinned anchor,
//! challenge/app/authorization-list policy — mirrors the production checks and
//! runs against deterministic synthetic fixtures.

use der::Encode;
use ed25519_dalek::{Signature, VerifyingKey};
use subtle::ConstantTimeEq;

/// On-wire DER structures for the synthetic attestation chain.
pub mod wire {
    use der::asn1::OctetString;

    /// The attested facts carried by a leaf certificate. Mirrors the fields of
    /// an Android `KeyDescription` that the policy cares about.
    #[derive(Clone, der::Sequence)]
    pub struct AttestationRecord {
        /// Server-issued challenge the attestation is bound to.
        pub challenge: OctetString,
        /// Attesting application identity (package + signing digest).
        pub app_id: OctetString,
        /// Security level: 0 = software, 1 = TEE, 2 = StrongBox.
        pub security_level: u8,
        /// Whether the key requires user authentication before each use.
        pub user_auth_required: bool,
        /// Whether the key may be used with no user authentication at all.
        pub no_auth_required: bool,
        /// Auth validity window in seconds; `0` means per-use (no timeout).
        pub auth_timeout_secs: u32,
        /// The control public key this attestation vouches for.
        pub attested_public_key: OctetString,
    }

    /// To-be-signed body of a certificate.
    #[derive(Clone, der::Sequence)]
    pub struct TbsCert {
        pub version: u8,
        pub subject: OctetString,
        pub subject_public_key: OctetString,
        pub issuer: OctetString,
        /// Present only on a leaf certificate.
        pub attestation: Option<AttestationRecord>,
    }

    /// A certificate: a signed `TbsCert`.
    #[derive(Clone, der::Sequence)]
    pub struct Cert {
        pub tbs: TbsCert,
        /// Ed25519 signature over `DER(tbs)` produced by the issuer key.
        pub signature: OctetString,
    }
}

/// Hardware security level of an attested key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecurityLevel {
    Software = 0,
    Tee = 1,
    StrongBox = 2,
}

impl SecurityLevel {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Software),
            1 => Some(Self::Tee),
            2 => Some(Self::StrongBox),
            _ => None,
        }
    }
}

/// Trust policy the gateway enforces on control-key attestations.
#[derive(Clone)]
pub struct AttestationPolicy {
    /// Pinned Ed25519 public key of the attestation root (the trust anchor).
    pub trust_anchor: [u8; 32],
    /// Application identity that must have produced the attestation.
    pub expected_app_id: Vec<u8>,
    /// Minimum acceptable hardware security level (TEE or stronger).
    pub min_security_level: SecurityLevel,
}

/// Verified facts recorded for a device once attestation passes. Deliberately
/// carries no secret material — safe to persist beside the device record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestationFacts {
    pub security_level: SecurityLevel,
    pub app_id: Vec<u8>,
}

/// Reasons a control-key attestation is rejected. Messages are metadata only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttestationError {
    /// The chain was empty.
    EmptyChain,
    /// A certificate signature did not verify, a key was malformed, or an
    /// issuer/subject link was broken.
    BadChain,
    /// The chain verified but its root is not the pinned trust anchor.
    UntrustedAnchor,
    /// The leaf certificate carried no attestation record.
    MissingAttestation,
    /// The attested key is below the required security level.
    WeakSecurityLevel,
    /// The attesting application identity did not match policy.
    WrongApp,
    /// The attestation was not bound to the expected server challenge.
    WrongChallenge,
    /// The control key does not require user authentication for every use.
    UnrestrictedControlKey,
    /// The attested key is not the key being enrolled.
    KeyMismatch,
}

impl std::fmt::Display for AttestationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::EmptyChain => "attestation chain was empty",
            Self::BadChain => "attestation chain did not verify",
            Self::UntrustedAnchor => "attestation chain is not rooted in the pinned anchor",
            Self::MissingAttestation => "leaf certificate carried no attestation record",
            Self::WeakSecurityLevel => "attested key is below the required security level",
            Self::WrongApp => "attesting application identity is not permitted",
            Self::WrongChallenge => "attestation is not bound to the expected challenge",
            Self::UnrestrictedControlKey => "control key does not require per-use authentication",
            Self::KeyMismatch => "attested key does not match the enrolled control key",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for AttestationError {}

/// The most certificates an attestation chain may contain. A genuine Android
/// chain is leaf → intermediate(s) → root, a handful deep; anything longer is
/// rejected before per-certificate verification runs.
pub const MAX_ATTESTATION_CHAIN_LEN: usize = 10;

/// Verify a control-key attestation chain against `policy`, binding it to
/// `expected_challenge` and the `enrolled_control_key` being paired.
///
/// The chain is ordered leaf-first: `chain[0]` is the attesting leaf and the
/// final element is the self-signed root, which must equal the pinned anchor.
pub fn verify_control_attestation(
    chain: &[wire::Cert],
    policy: &AttestationPolicy,
    expected_challenge: &[u8],
    enrolled_control_key: &[u8; 32],
) -> Result<AttestationFacts, AttestationError> {
    if chain.is_empty() {
        return Err(AttestationError::EmptyChain);
    }
    // Bound the chain before doing any per-certificate signature work: a real
    // Android attestation chain is a handful of certificates, so an over-long
    // chain is malformed and must not be allowed to drive unbounded verification.
    if chain.len() > MAX_ATTESTATION_CHAIN_LEN {
        return Err(AttestationError::BadChain);
    }

    // Each certificate must be signed by the next one up, and the
    // issuer/subject names must link.
    for pair in chain.windows(2) {
        let child = &pair[0];
        let issuer = &pair[1];
        let issuer_key = parse_key(&issuer.tbs.subject_public_key)?;
        verify_cert_signature(child, &issuer_key)?;
        if child.tbs.issuer.as_bytes() != issuer.tbs.subject.as_bytes() {
            return Err(AttestationError::BadChain);
        }
    }

    // The root must be self-signed and equal to the pinned trust anchor.
    let root = chain.last().expect("chain is non-empty");
    let root_key = parse_key(&root.tbs.subject_public_key)?;
    verify_cert_signature(root, &root_key)?;
    if root_key.to_bytes().ct_eq(&policy.trust_anchor).unwrap_u8() != 1 {
        return Err(AttestationError::UntrustedAnchor);
    }

    // Policy checks on the leaf's attestation record.
    let leaf = &chain[0];
    let record = leaf
        .tbs
        .attestation
        .as_ref()
        .ok_or(AttestationError::MissingAttestation)?;

    let level =
        SecurityLevel::from_u8(record.security_level).ok_or(AttestationError::WeakSecurityLevel)?;
    if level < policy.min_security_level {
        return Err(AttestationError::WeakSecurityLevel);
    }
    if !ct_eq(record.app_id.as_bytes(), &policy.expected_app_id) {
        return Err(AttestationError::WrongApp);
    }
    if !ct_eq(record.challenge.as_bytes(), expected_challenge) {
        return Err(AttestationError::WrongChallenge);
    }
    // Auth-per-use: the key must require user authentication, must not be
    // usable without auth, and must not use a time-bounded auth window.
    let per_use =
        record.user_auth_required && !record.no_auth_required && record.auth_timeout_secs == 0;
    if !per_use {
        return Err(AttestationError::UnrestrictedControlKey);
    }
    if !ct_eq(record.attested_public_key.as_bytes(), enrolled_control_key) {
        return Err(AttestationError::KeyMismatch);
    }

    Ok(AttestationFacts {
        security_level: level,
        app_id: record.app_id.as_bytes().to_vec(),
    })
}

/// Verify that `cert.signature` is a valid Ed25519 signature over `DER(tbs)`
/// under `issuer_key`.
fn verify_cert_signature(
    cert: &wire::Cert,
    issuer_key: &VerifyingKey,
) -> Result<(), AttestationError> {
    let tbs = cert.tbs.to_der().map_err(|_| AttestationError::BadChain)?;
    let signature =
        Signature::from_slice(cert.signature.as_bytes()).map_err(|_| AttestationError::BadChain)?;
    issuer_key
        .verify_strict(&tbs, &signature)
        .map_err(|_| AttestationError::BadChain)
}

/// Parse a 32-byte Ed25519 public key out of an octet string.
fn parse_key(oct: &der::asn1::OctetString) -> Result<VerifyingKey, AttestationError> {
    let bytes: [u8; 32] = oct
        .as_bytes()
        .try_into()
        .map_err(|_| AttestationError::BadChain)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| AttestationError::BadChain)
}

/// Length-checked constant-time byte comparison. Length is not secret; content
/// comparison is constant time to avoid leaking how far a match proceeded.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.ct_eq(b).unwrap_u8() == 1
}
