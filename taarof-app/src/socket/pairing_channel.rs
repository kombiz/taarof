//! Local operator pairing-confirmation channel.
//!
//! The control gateway owns pairing crypto, attestation, and the authoritative
//! device records. This is the desktop side of the same-user confirmation
//! channel the design spec calls for: it tracks short-lived offers the operator
//! created, devices the gateway surfaces as pending, and the operator's
//! confirm / reject / revoke decisions. It holds no key material and makes no
//! trust decisions of its own — it is the UI-facing mirror the local socket
//! exposes so the operator can approve or deny a pairing.

/// Local mirror of the gateway's five-minute pairing-offer lifetime.
pub const LOCAL_OFFER_TTL_MS: u64 = 300_000;

/// A device the gateway has surfaced as pending against an offer, shown to the
/// operator for explicit confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDeviceInfo {
    pub device_name: String,
    pub key_fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferDecision {
    Confirmed,
    Rejected,
}

#[derive(Debug)]
struct LocalOffer {
    offer_id: String,
    expires_at_ms: u64,
    pending: Option<PendingDeviceInfo>,
    decision: Option<OfferDecision>,
}

#[derive(Debug)]
struct LocalDevice {
    device_id: String,
    revoked: bool,
}

/// A pending device the operator can act on, projected for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingView {
    pub offer_id: String,
    pub device_name: String,
    pub key_fingerprint: String,
}

/// Why a pairing-channel operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingChannelError {
    UnknownOffer,
    OfferExpired,
    NoPendingDevice,
    AlreadyDecided,
    UnknownDevice,
}

impl PairingChannelError {
    /// Operator-facing message. Carries no key material.
    pub fn message(self) -> &'static str {
        match self {
            Self::UnknownOffer => "no such pairing offer",
            Self::OfferExpired => "pairing offer has expired",
            Self::NoPendingDevice => "offer has no pending device to confirm",
            Self::AlreadyDecided => "offer has already been confirmed or rejected",
            Self::UnknownDevice => "no such device",
        }
    }
}

/// Desktop-side pairing state exposed over the local Unix socket.
#[derive(Debug, Default)]
pub struct PairingCoordinator {
    offers: Vec<LocalOffer>,
    devices: Vec<LocalDevice>,
}

impl PairingCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the operator asked the gateway for a pairing offer.
    /// Returns the offer's expiry in Unix milliseconds.
    pub fn create_offer(&mut self, offer_id: String, now_ms: u64) -> u64 {
        let expires_at_ms = now_ms.saturating_add(LOCAL_OFFER_TTL_MS);
        self.offers.push(LocalOffer {
            offer_id,
            expires_at_ms,
            pending: None,
            decision: None,
        });
        expires_at_ms
    }

    /// Gateway-driven: surface a device that enrolled against `offer_id` so the
    /// operator can confirm or reject it.
    pub fn attach_pending(
        &mut self,
        offer_id: &str,
        info: PendingDeviceInfo,
    ) -> Result<(), PairingChannelError> {
        self.offer_mut(offer_id)?.pending = Some(info);
        Ok(())
    }

    /// Devices awaiting operator action: pending, undecided, unexpired offers.
    pub fn pending(&self, now_ms: u64) -> Vec<PendingView> {
        self.offers
            .iter()
            .filter(|o| o.decision.is_none() && now_ms <= o.expires_at_ms)
            .filter_map(|o| {
                o.pending.as_ref().map(|p| PendingView {
                    offer_id: o.offer_id.clone(),
                    device_name: p.device_name.clone(),
                    key_fingerprint: p.key_fingerprint.clone(),
                })
            })
            .collect()
    }

    /// Confirm the pending device on `offer_id`, recording it as an active
    /// device under `device_id`. Returns the confirmed device id.
    pub fn confirm(
        &mut self,
        offer_id: &str,
        device_id: String,
        now_ms: u64,
    ) -> Result<String, PairingChannelError> {
        let offer = self.offer_mut(offer_id)?;
        if offer.decision.is_some() {
            return Err(PairingChannelError::AlreadyDecided);
        }
        if now_ms > offer.expires_at_ms {
            return Err(PairingChannelError::OfferExpired);
        }
        if offer.pending.is_none() {
            return Err(PairingChannelError::NoPendingDevice);
        }
        offer.decision = Some(OfferDecision::Confirmed);
        self.devices.push(LocalDevice {
            device_id: device_id.clone(),
            revoked: false,
        });
        Ok(device_id)
    }

    /// Reject the pending device on `offer_id`.
    pub fn reject(&mut self, offer_id: &str) -> Result<(), PairingChannelError> {
        let offer = self.offer_mut(offer_id)?;
        if offer.decision.is_some() {
            return Err(PairingChannelError::AlreadyDecided);
        }
        offer.decision = Some(OfferDecision::Rejected);
        offer.pending = None;
        Ok(())
    }

    /// Revoke an active device.
    pub fn revoke(&mut self, device_id: &str) -> Result<(), PairingChannelError> {
        let device = self
            .devices
            .iter_mut()
            .find(|d| d.device_id == device_id)
            .ok_or(PairingChannelError::UnknownDevice)?;
        device.revoked = true;
        Ok(())
    }

    /// Whether a device is currently paired and not revoked.
    pub fn is_active_device(&self, device_id: &str) -> bool {
        self.devices
            .iter()
            .any(|d| d.device_id == device_id && !d.revoked)
    }

    fn offer_mut(&mut self, offer_id: &str) -> Result<&mut LocalOffer, PairingChannelError> {
        self.offers
            .iter_mut()
            .find(|o| o.offer_id == offer_id)
            .ok_or(PairingChannelError::UnknownOffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> PendingDeviceInfo {
        PendingDeviceInfo {
            device_name: name.to_string(),
            key_fingerprint: format!("sha256:{}", "0".repeat(64)),
        }
    }

    #[test]
    fn confirm_flow_records_active_device() {
        let mut c = PairingCoordinator::new();
        let now = 1_000;
        c.create_offer("offer-1".into(), now);
        c.attach_pending("offer-1", info("Pixel")).unwrap();

        assert_eq!(c.pending(now).len(), 1);
        let device_id = c.confirm("offer-1", "device-1".into(), now).unwrap();
        assert_eq!(device_id, "device-1");
        assert!(c.is_active_device("device-1"));
        assert!(c.pending(now).is_empty(), "confirmed offer leaves pending");
    }

    #[test]
    fn reject_flow_leaves_no_active_device() {
        let mut c = PairingCoordinator::new();
        c.create_offer("offer-1".into(), 0);
        c.attach_pending("offer-1", info("Pixel")).unwrap();
        c.reject("offer-1").unwrap();
        assert!(c.pending(0).is_empty());
        assert_eq!(
            c.confirm("offer-1", "device-1".into(), 0),
            Err(PairingChannelError::AlreadyDecided)
        );
    }

    #[test]
    fn expired_offer_cannot_be_confirmed() {
        let mut c = PairingCoordinator::new();
        c.create_offer("offer-1".into(), 0);
        c.attach_pending("offer-1", info("Pixel")).unwrap();
        let too_late = LOCAL_OFFER_TTL_MS + 1;
        assert_eq!(
            c.confirm("offer-1", "device-1".into(), too_late),
            Err(PairingChannelError::OfferExpired)
        );
        assert!(
            c.pending(too_late).is_empty(),
            "expired offer is not pending"
        );
    }

    #[test]
    fn confirm_without_pending_is_rejected() {
        let mut c = PairingCoordinator::new();
        c.create_offer("offer-1".into(), 0);
        assert_eq!(
            c.confirm("offer-1", "device-1".into(), 0),
            Err(PairingChannelError::NoPendingDevice)
        );
    }

    #[test]
    fn revoke_deactivates_device() {
        let mut c = PairingCoordinator::new();
        c.create_offer("offer-1".into(), 0);
        c.attach_pending("offer-1", info("Pixel")).unwrap();
        c.confirm("offer-1", "device-1".into(), 0).unwrap();
        c.revoke("device-1").unwrap();
        assert!(!c.is_active_device("device-1"));
        assert_eq!(c.revoke("nope"), Err(PairingChannelError::UnknownDevice));
    }

    #[test]
    fn unknown_offer_is_rejected() {
        let mut c = PairingCoordinator::new();
        assert_eq!(
            c.attach_pending("ghost", info("Pixel")),
            Err(PairingChannelError::UnknownOffer)
        );
        assert_eq!(c.reject("ghost"), Err(PairingChannelError::UnknownOffer));
    }
}
