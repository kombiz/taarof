//! Grant policy: the three separate authorization scopes and their distinct
//! lifetimes, limits, and revocation cascade.
//!
//! Per the design spec these are *separate* grants with *separate* lifetimes,
//! not tiers of one token:
//!
//! - **Observe** — state reads, pane output, navigation. Renewable, lasts at
//!   most one hour.
//! - **Pane control** — input, paste, resize for exactly one pane. Expires after
//!   fifteen minutes of inactivity and can never outlive its observe grant.
//! - **Runtime mutation** — tab creation (and, with a target pane, split
//!   creation). Needs no pane, lasts five minutes, and is discarded immediately
//!   on backgrounding.
//!
//! A device may attach at most four panes and write to at most one. Changing the
//! observed pane revokes pane control but leaves runtime mutation intact;
//! backgrounding, losing observe, or device revocation discards control at once.
//!
//! This module is a pure decision core over `now_ms` and identifiers: no
//! terminal content, no crypto, no I/O. The relay drives it; [`crate::auth`]
//! remains responsible for the signatures that authenticate each request.

use std::collections::HashMap;

pub use crate::auth::Scope;

/// Observe grants last at most one hour before they must be renewed.
pub const OBSERVE_TTL_MS: u64 = 3_600_000;
/// Pane control lapses after fifteen minutes without a control operation.
pub const PANE_CONTROL_INACTIVITY_MS: u64 = 900_000;
/// Runtime mutation lasts five minutes from grant.
pub const RUNTIME_MUTATION_TTL_MS: u64 = 300_000;

/// The most panes one device may hold attached at once.
pub const MAX_ATTACHED_PANES: usize = 4;

/// Default metadata-request budget: 120 requests per rolling minute.
pub const METADATA_REQUESTS_PER_MINUTE: u32 = 120;
/// Default control-operation budget: 30 operations per rolling second.
pub const CONTROL_OPERATIONS_PER_SECOND: u32 = 30;

const MINUTE_MS: u64 = 60_000;
const SECOND_MS: u64 = 1_000;

/// A rolling-window counter. Records event timestamps and rejects once more than
/// `limit` fall inside `window_ms`.
#[derive(Default)]
struct RateWindow {
    events: Vec<u64>,
}

impl RateWindow {
    /// Prune events outside the window, then admit one if under `limit`.
    fn admit(&mut self, now_ms: u64, window_ms: u64, limit: u32) -> bool {
        let cutoff = now_ms.saturating_sub(window_ms);
        self.events.retain(|&t| t > cutoff);
        if self.events.len() as u32 >= limit {
            return false;
        }
        self.events.push(now_ms);
        true
    }
}

/// A pane's logical address. Never a filesystem path or PID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneKey {
    pub tab: String,
    pub pane: String,
}

impl PaneKey {
    pub fn new(tab: impl Into<String>, pane: impl Into<String>) -> Self {
        Self {
            tab: tab.into(),
            pane: pane.into(),
        }
    }
}

/// Why a grant operation was denied. Metadata only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// No live observe grant backs this operation.
    NoObserve,
    /// The device already holds the maximum number of attached panes.
    TooManyAttachedPanes,
    /// The pane is not attached under the current observe grant.
    PaneNotAttached,
    /// No pane-control grant is held for the requested pane.
    NoPaneControl,
    /// A pane-control grant exists but for a different pane (one-writer rule).
    WrongWriter,
    /// The pane-control grant lapsed after fifteen minutes of inactivity.
    PaneControlExpired,
    /// The presented control grant generation does not match the live one.
    GenerationMismatch,
    /// No live runtime-mutation grant is held.
    NoRuntimeMutation,
    /// The runtime-mutation grant has expired.
    RuntimeMutationExpired,
    /// The device exceeded its request/operation rate budget.
    RateLimited,
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::NoObserve => "no live observe grant",
            Self::TooManyAttachedPanes => "device already holds the maximum attached panes",
            Self::PaneNotAttached => "pane is not attached under the observe grant",
            Self::NoPaneControl => "no pane-control grant for this pane",
            Self::WrongWriter => "pane control is held for a different pane",
            Self::PaneControlExpired => "pane control expired after inactivity",
            Self::GenerationMismatch => "control grant generation does not match",
            Self::NoRuntimeMutation => "no live runtime-mutation grant",
            Self::RuntimeMutationExpired => "runtime-mutation grant expired",
            Self::RateLimited => "device exceeded its request rate budget",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for GrantError {}

struct ObserveGrant {
    expires_at_ms: u64,
}

struct PaneControlGrant {
    pane: PaneKey,
    generation: u64,
    last_activity_ms: u64,
}

struct RuntimeMutationGrant {
    expires_at_ms: u64,
}

/// All grants held by one device. Constructed only through the ledger.
#[derive(Default)]
struct DeviceGrants {
    observe: Option<ObserveGrant>,
    attached: Vec<PaneKey>,
    writer: Option<PaneControlGrant>,
    runtime_mutation: Option<RuntimeMutationGrant>,
    /// Monotonic source for control grant generations, so a replaced grant never
    /// reuses a generation an in-flight frame might still carry.
    next_generation: u64,
    /// Rolling budget for metadata requests (attach, navigation, grants).
    metadata_rate: RateWindow,
    /// Rolling budget for control operations (input, resize, tab/split create).
    control_rate: RateWindow,
}

impl DeviceGrants {
    fn observe_live(&self, now_ms: u64) -> bool {
        self.observe
            .as_ref()
            .is_some_and(|g| now_ms <= g.expires_at_ms)
    }
}

/// Per-device grant ledger. One instance backs the whole gateway; the relay
/// keys into it by device UUID.
#[derive(Default)]
pub struct GrantLedger {
    devices: HashMap<String, DeviceGrants>,
}

impl GrantLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&mut self, device: &str) -> &mut DeviceGrants {
        self.devices.entry(device.to_string()).or_default()
    }

    /// Open or renew an observe grant. Renewal never extends control past its own
    /// limits; it only refreshes the one-hour observe ceiling.
    pub fn open_observe(&mut self, device: &str, now_ms: u64) {
        let expires_at_ms = now_ms.saturating_add(OBSERVE_TTL_MS);
        self.entry(device).observe = Some(ObserveGrant { expires_at_ms });
    }

    /// Whether the device currently holds a live observe grant.
    pub fn observe_live(&self, device: &str, now_ms: u64) -> bool {
        self.devices
            .get(device)
            .is_some_and(|g| g.observe_live(now_ms))
    }

    /// Attach a pane under the observe grant. Enforces the four-attach limit;
    /// re-attaching an already-attached pane is idempotent.
    pub fn attach_pane(
        &mut self,
        device: &str,
        now_ms: u64,
        pane: &PaneKey,
    ) -> Result<(), GrantError> {
        let grants = self.entry(device);
        if !grants.observe_live(now_ms) {
            return Err(GrantError::NoObserve);
        }
        if grants.attached.contains(pane) {
            return Ok(());
        }
        if grants.attached.len() >= MAX_ATTACHED_PANES {
            return Err(GrantError::TooManyAttachedPanes);
        }
        grants.attached.push(pane.clone());
        Ok(())
    }

    /// Detach a pane; also relinquishes pane control if it was the writer.
    pub fn detach_pane(&mut self, device: &str, pane: &PaneKey) {
        if let Some(grants) = self.devices.get_mut(device) {
            grants.attached.retain(|p| p != pane);
            if grants.writer.as_ref().is_some_and(|w| &w.pane == pane) {
                grants.writer = None;
            }
        }
    }

    /// The panes currently attached by the device, in attach order.
    pub fn attached_panes(&self, device: &str) -> Vec<PaneKey> {
        self.devices
            .get(device)
            .map(|g| g.attached.clone())
            .unwrap_or_default()
    }

    /// Grant pane control for `pane`, replacing any existing writer (one-writer
    /// rule). Returns the new control grant generation. Requires a live observe
    /// grant and the pane to be attached.
    pub fn grant_pane_control(
        &mut self,
        device: &str,
        now_ms: u64,
        pane: &PaneKey,
    ) -> Result<u64, GrantError> {
        let grants = self.entry(device);
        if !grants.observe_live(now_ms) {
            return Err(GrantError::NoObserve);
        }
        if !grants.attached.contains(pane) {
            return Err(GrantError::PaneNotAttached);
        }
        let generation = grants.next_generation;
        grants.next_generation = grants.next_generation.saturating_add(1);
        grants.writer = Some(PaneControlGrant {
            pane: pane.clone(),
            generation,
            last_activity_ms: now_ms,
        });
        Ok(generation)
    }

    /// Verify a pane-control operation is authorized: live observe, the writer is
    /// this pane, the grant matches `generation`, and it has not lapsed. On
    /// success the inactivity timer is refreshed (this call *is* activity).
    pub fn use_pane_control(
        &mut self,
        device: &str,
        now_ms: u64,
        pane: &PaneKey,
        generation: u64,
    ) -> Result<(), GrantError> {
        let grants = self.devices.get_mut(device).ok_or(GrantError::NoObserve)?;
        if !grants.observe_live(now_ms) {
            // Control can never outlive observe.
            grants.writer = None;
            return Err(GrantError::NoObserve);
        }
        let writer = grants.writer.as_mut().ok_or(GrantError::NoPaneControl)?;
        if &writer.pane != pane {
            return Err(GrantError::WrongWriter);
        }
        if now_ms
            > writer
                .last_activity_ms
                .saturating_add(PANE_CONTROL_INACTIVITY_MS)
        {
            grants.writer = None;
            return Err(GrantError::PaneControlExpired);
        }
        if writer.generation != generation {
            return Err(GrantError::GenerationMismatch);
        }
        writer.last_activity_ms = now_ms;
        Ok(())
    }

    /// Change the observed pane: revokes pane control (a new pane must be
    /// re-authorized) but deliberately leaves runtime mutation intact.
    pub fn change_observed_pane(&mut self, device: &str) {
        if let Some(grants) = self.devices.get_mut(device) {
            grants.writer = None;
        }
    }

    /// Grant runtime mutation (tab/split creation). Requires a live observe
    /// grant. Returns the grant generation.
    pub fn grant_runtime_mutation(&mut self, device: &str, now_ms: u64) -> Result<u64, GrantError> {
        let grants = self.entry(device);
        if !grants.observe_live(now_ms) {
            return Err(GrantError::NoObserve);
        }
        let generation = grants.next_generation;
        grants.next_generation = grants.next_generation.saturating_add(1);
        grants.runtime_mutation = Some(RuntimeMutationGrant {
            expires_at_ms: now_ms.saturating_add(RUNTIME_MUTATION_TTL_MS),
        });
        Ok(generation)
    }

    /// Verify a runtime-mutation operation is authorized: live observe and a live
    /// runtime-mutation grant.
    pub fn use_runtime_mutation(&mut self, device: &str, now_ms: u64) -> Result<(), GrantError> {
        let grants = self.devices.get_mut(device).ok_or(GrantError::NoObserve)?;
        if !grants.observe_live(now_ms) {
            grants.runtime_mutation = None;
            return Err(GrantError::NoObserve);
        }
        let grant = grants
            .runtime_mutation
            .as_ref()
            .ok_or(GrantError::NoRuntimeMutation)?;
        if now_ms > grant.expires_at_ms {
            grants.runtime_mutation = None;
            return Err(GrantError::RuntimeMutationExpired);
        }
        Ok(())
    }

    /// Charge one metadata request against the device's rolling minute budget.
    /// Returns `RateLimited` once the budget is exhausted.
    pub fn check_metadata_rate(&mut self, device: &str, now_ms: u64) -> Result<(), GrantError> {
        if self
            .entry(device)
            .metadata_rate
            .admit(now_ms, MINUTE_MS, METADATA_REQUESTS_PER_MINUTE)
        {
            Ok(())
        } else {
            Err(GrantError::RateLimited)
        }
    }

    /// Charge one control operation against the device's rolling second budget.
    pub fn check_control_rate(&mut self, device: &str, now_ms: u64) -> Result<(), GrantError> {
        if self
            .entry(device)
            .control_rate
            .admit(now_ms, SECOND_MS, CONTROL_OPERATIONS_PER_SECOND)
        {
            Ok(())
        } else {
            Err(GrantError::RateLimited)
        }
    }

    /// Backgrounding the app: discard pane control and runtime mutation at once.
    /// Observe survives (it reconnects automatically).
    pub fn background(&mut self, device: &str) {
        if let Some(grants) = self.devices.get_mut(device) {
            grants.writer = None;
            grants.runtime_mutation = None;
        }
    }

    /// Losing the observe grant (or device revocation): discard everything.
    pub fn revoke_all(&mut self, device: &str) {
        self.devices.remove(device);
    }

    /// Reclaim grant state that can no longer be used. A device whose observe
    /// grant has lapsed holds nothing usable (control and runtime mutation can
    /// never outlive observe), so its whole entry is dropped; survivors have any
    /// expired runtime-mutation grant or inactive writer cleared. Mirrors
    /// [`crate::auth::SessionStore::sweep_expired`]: a memory-reclamation sweep,
    /// not a security boundary — every `use_*` already refuses an expired grant.
    /// Returns the number of device entries removed.
    pub fn sweep_expired(&mut self, now_ms: u64) -> usize {
        let before = self.devices.len();
        self.devices.retain(|_, g| g.observe_live(now_ms));
        for grants in self.devices.values_mut() {
            if grants
                .runtime_mutation
                .as_ref()
                .is_some_and(|rt| now_ms > rt.expires_at_ms)
            {
                grants.runtime_mutation = None;
            }
            if grants.writer.as_ref().is_some_and(|w| {
                now_ms
                    > w.last_activity_ms
                        .saturating_add(PANE_CONTROL_INACTIVITY_MS)
            }) {
                grants.writer = None;
            }
        }
        before - self.devices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "device-uuid-1";
    const T0: u64 = 1_000_000;

    fn pane(n: &str) -> PaneKey {
        PaneKey::new("tab-1", n)
    }

    #[test]
    fn observe_expires_after_one_hour() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        assert!(ledger.observe_live(DEVICE, T0 + OBSERVE_TTL_MS));
        assert!(!ledger.observe_live(DEVICE, T0 + OBSERVE_TTL_MS + 1));
    }

    #[test]
    fn attach_enforces_four_pane_limit() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        for n in 0..MAX_ATTACHED_PANES {
            ledger
                .attach_pane(DEVICE, T0, &pane(&n.to_string()))
                .expect("first four attaches succeed");
        }
        assert_eq!(
            ledger.attach_pane(DEVICE, T0, &pane("overflow")),
            Err(GrantError::TooManyAttachedPanes)
        );
        // Re-attaching an existing pane is idempotent, not a limit breach.
        ledger
            .attach_pane(DEVICE, T0, &pane("0"))
            .expect("re-attach is idempotent");
    }

    #[test]
    fn attach_requires_live_observe() {
        let mut ledger = GrantLedger::new();
        assert_eq!(
            ledger.attach_pane(DEVICE, T0, &pane("a")),
            Err(GrantError::NoObserve)
        );
    }

    #[test]
    fn one_writer_only_the_latest_grant_wins() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        ledger.attach_pane(DEVICE, T0, &pane("b")).unwrap();
        let gen_a = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        let gen_b = ledger.grant_pane_control(DEVICE, T0, &pane("b")).unwrap();
        assert_ne!(gen_a, gen_b, "each grant gets a fresh generation");
        // The old writer is no longer authorized once control moved to pane b.
        assert_eq!(
            ledger.use_pane_control(DEVICE, T0, &pane("a"), gen_a),
            Err(GrantError::WrongWriter)
        );
        ledger
            .use_pane_control(DEVICE, T0, &pane("b"), gen_b)
            .expect("current writer is authorized");
    }

    #[test]
    fn pane_control_expires_after_inactivity() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let generation = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        let idle = T0 + PANE_CONTROL_INACTIVITY_MS + 1;
        assert_eq!(
            ledger.use_pane_control(DEVICE, idle, &pane("a"), generation),
            Err(GrantError::PaneControlExpired)
        );
    }

    #[test]
    fn activity_refreshes_the_inactivity_timer() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let generation = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        let mid = T0 + PANE_CONTROL_INACTIVITY_MS - 1;
        ledger
            .use_pane_control(DEVICE, mid, &pane("a"), generation)
            .expect("use just before expiry refreshes the timer");
        // Fifteen minutes after the *refresh*, still live.
        let later = mid + PANE_CONTROL_INACTIVITY_MS - 1;
        ledger
            .use_pane_control(DEVICE, later, &pane("a"), generation)
            .expect("timer was refreshed by the prior use");
    }

    #[test]
    fn generation_mismatch_is_rejected() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let generation = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        assert_eq!(
            ledger.use_pane_control(DEVICE, T0, &pane("a"), generation + 99),
            Err(GrantError::GenerationMismatch)
        );
    }

    #[test]
    fn pane_control_cannot_outlive_observe() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let generation = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        // Past the observe ceiling but well within the control inactivity window.
        let after_observe = T0 + OBSERVE_TTL_MS + 1;
        assert_eq!(
            ledger.use_pane_control(DEVICE, after_observe, &pane("a"), generation),
            Err(GrantError::NoObserve)
        );
    }

    #[test]
    fn changing_pane_revokes_control_but_keeps_runtime_mutation() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let control = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        ledger.grant_runtime_mutation(DEVICE, T0).unwrap();

        ledger.change_observed_pane(DEVICE);

        assert_eq!(
            ledger.use_pane_control(DEVICE, T0, &pane("a"), control),
            Err(GrantError::NoPaneControl)
        );
        ledger
            .use_runtime_mutation(DEVICE, T0)
            .expect("runtime mutation survives a pane change");
    }

    #[test]
    fn runtime_mutation_expires_after_five_minutes() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.grant_runtime_mutation(DEVICE, T0).unwrap();
        assert_eq!(
            ledger.use_runtime_mutation(DEVICE, T0 + RUNTIME_MUTATION_TTL_MS + 1),
            Err(GrantError::RuntimeMutationExpired)
        );
    }

    #[test]
    fn backgrounding_discards_control_and_runtime_mutation_but_not_observe() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        let control = ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        ledger.grant_runtime_mutation(DEVICE, T0).unwrap();

        ledger.background(DEVICE);

        assert_eq!(
            ledger.use_pane_control(DEVICE, T0, &pane("a"), control),
            Err(GrantError::NoPaneControl)
        );
        assert_eq!(
            ledger.use_runtime_mutation(DEVICE, T0),
            Err(GrantError::NoRuntimeMutation)
        );
        assert!(
            ledger.observe_live(DEVICE, T0),
            "observe survives background"
        );
    }

    #[test]
    fn control_rate_limit_rejects_the_thirty_first_operation_in_a_second() {
        let mut ledger = GrantLedger::new();
        for _ in 0..CONTROL_OPERATIONS_PER_SECOND {
            ledger
                .check_control_rate(DEVICE, T0)
                .expect("first 30 control ops in the second are admitted");
        }
        assert_eq!(
            ledger.check_control_rate(DEVICE, T0),
            Err(GrantError::RateLimited)
        );
        // A second later the window has rolled and the budget refreshes.
        ledger
            .check_control_rate(DEVICE, T0 + 1_000)
            .expect("the budget refreshes after the window rolls");
    }

    #[test]
    fn metadata_rate_limit_rejects_beyond_the_minute_budget() {
        let mut ledger = GrantLedger::new();
        for _ in 0..METADATA_REQUESTS_PER_MINUTE {
            ledger.check_metadata_rate(DEVICE, T0).unwrap();
        }
        assert_eq!(
            ledger.check_metadata_rate(DEVICE, T0),
            Err(GrantError::RateLimited)
        );
    }

    #[test]
    fn sweep_expired_removes_devices_whose_observe_lapsed() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe("live", T0);
        ledger.open_observe("dead", T0);
        let later = T0 + OBSERVE_TTL_MS + 1;
        ledger.open_observe("live", later); // renew the survivor

        assert_eq!(
            ledger.sweep_expired(later),
            1,
            "the lapsed device is reclaimed"
        );
        assert!(ledger.observe_live("live", later));
        assert!(!ledger.observe_live("dead", later));
    }

    #[test]
    fn sweep_expired_clears_an_expired_runtime_mutation_on_a_live_device() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe("d", T0);
        ledger.grant_runtime_mutation("d", T0).unwrap();
        let later = T0 + RUNTIME_MUTATION_TTL_MS + 1;
        assert_eq!(ledger.sweep_expired(later), 0, "the device itself survives");
        assert_eq!(
            ledger.use_runtime_mutation("d", later),
            Err(GrantError::NoRuntimeMutation),
            "the expired runtime-mutation grant was cleared"
        );
    }

    #[test]
    fn revoke_all_discards_every_grant() {
        let mut ledger = GrantLedger::new();
        ledger.open_observe(DEVICE, T0);
        ledger.attach_pane(DEVICE, T0, &pane("a")).unwrap();
        ledger.grant_pane_control(DEVICE, T0, &pane("a")).unwrap();
        ledger.revoke_all(DEVICE);
        assert!(!ledger.observe_live(DEVICE, T0));
        assert!(ledger.attached_panes(DEVICE).is_empty());
    }
}
