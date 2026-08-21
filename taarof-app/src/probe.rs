use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProbeState {
    Ok,
    Stale,
    #[default]
    Unknown,
    Error,
}

impl ProbeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
            Self::Error => "error",
        }
    }

    pub fn is_degraded(self) -> bool {
        matches!(self, Self::Stale | Self::Error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeTransition {
    pub previous: ProbeState,
    pub current: ProbeState,
}

impl ProbeTransition {
    pub fn changed(&self) -> bool {
        self.previous != self.current
    }

    pub fn entered_degraded(&self) -> bool {
        !self.previous.is_degraded() && self.current.is_degraded()
    }

    pub fn recovered(&self) -> bool {
        self.previous.is_degraded() && matches!(self.current, ProbeState::Ok)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProbeSnapshot<T> {
    pub state: ProbeState,
    pub value: Option<T>,
    pub observed_at_unix_ms: Option<u64>,
    pub checked_at_unix_ms: Option<u64>,
    pub error: Option<String>,
}

impl<T> Default for ProbeSnapshot<T> {
    fn default() -> Self {
        Self::unknown()
    }
}

impl<T> ProbeSnapshot<T> {
    pub fn unknown() -> Self {
        Self {
            state: ProbeState::Unknown,
            value: None,
            observed_at_unix_ms: None,
            checked_at_unix_ms: None,
            error: None,
        }
    }

    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    pub fn unit_meta(&self) -> ProbeSnapshot<()> {
        ProbeSnapshot {
            state: self.state,
            value: self.value.as_ref().map(|_| ()),
            observed_at_unix_ms: self.observed_at_unix_ms,
            checked_at_unix_ms: self.checked_at_unix_ms,
            error: self.error.clone(),
        }
    }

    pub fn record_success(&mut self, value: T) -> ProbeTransition {
        let now = unix_time_ms();
        let transition = ProbeTransition {
            previous: self.state,
            current: ProbeState::Ok,
        };
        self.state = ProbeState::Ok;
        self.value = Some(value);
        self.observed_at_unix_ms = Some(now);
        self.checked_at_unix_ms = Some(now);
        self.error = None;
        transition
    }

    pub fn record_failure(&mut self, error: impl Into<String>) -> ProbeTransition {
        let now = unix_time_ms();
        let next_state = if self.value.is_some() {
            ProbeState::Stale
        } else {
            ProbeState::Error
        };
        let transition = ProbeTransition {
            previous: self.state,
            current: next_state,
        };
        self.state = next_state;
        self.checked_at_unix_ms = Some(now);
        self.error = Some(error.into());
        transition
    }

    /// Like [`Self::record_success`], but also reports whether the rendered
    /// content changed (state, error presence, or value differs). Callers use
    /// this to gate idle re-renders on an actual metadata diff rather than
    /// refreshing unconditionally on every poll.
    pub fn record_success_reporting(&mut self, value: T) -> (ProbeTransition, bool)
    where
        T: PartialEq,
    {
        let prev_state = self.state;
        let prev_error_present = self.error.is_some();
        let value_changed = self.value.as_ref() != Some(&value);
        let transition = self.record_success(value);
        let content_changed = value_changed || prev_state != ProbeState::Ok || prev_error_present;
        (transition, content_changed)
    }

    /// Like [`Self::record_failure`], but also reports whether the rendered
    /// content changed (state or error text differs).
    pub fn record_failure_reporting(
        &mut self,
        error: impl Into<String>,
    ) -> (ProbeTransition, bool) {
        let prev_state = self.state;
        let prev_error = self.error.clone();
        let transition = self.record_failure(error);
        let content_changed = prev_state != self.state || prev_error != self.error;
        (transition, content_changed)
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{ProbeSnapshot, ProbeState};

    #[test]
    fn failure_without_prior_value_becomes_error() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        let transition = probe.record_failure("boom");

        assert_eq!(transition.previous, ProbeState::Unknown);
        assert_eq!(transition.current, ProbeState::Error);
        assert_eq!(probe.state, ProbeState::Error);
        assert_eq!(probe.value, None);
        assert_eq!(probe.error.as_deref(), Some("boom"));
        assert!(probe.checked_at_unix_ms.is_some());
        assert_eq!(probe.observed_at_unix_ms, None);
    }

    #[test]
    fn failure_after_success_becomes_stale_and_keeps_last_value() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        probe.record_success(7);
        let observed_at = probe.observed_at_unix_ms;

        let transition = probe.record_failure("timeout");

        assert_eq!(transition.previous, ProbeState::Ok);
        assert_eq!(transition.current, ProbeState::Stale);
        assert_eq!(probe.state, ProbeState::Stale);
        assert_eq!(probe.value, Some(7));
        assert_eq!(probe.observed_at_unix_ms, observed_at);
        assert_eq!(probe.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn success_after_stale_recovers_and_clears_error() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        probe.record_success(7);
        probe.record_failure("timeout");

        let transition = probe.record_success(11);

        assert_eq!(transition.previous, ProbeState::Stale);
        assert_eq!(transition.current, ProbeState::Ok);
        assert_eq!(probe.state, ProbeState::Ok);
        assert_eq!(probe.value, Some(11));
        assert_eq!(probe.error, None);
        assert!(probe.checked_at_unix_ms.is_some());
        assert!(probe.observed_at_unix_ms.is_some());
    }

    #[test]
    fn unknown_to_ok_success_reports_change() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        let (_, changed) = probe.record_success_reporting(7);
        assert!(changed);
    }

    #[test]
    fn repeated_identical_success_reports_no_change() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        let (_, first) = probe.record_success_reporting(7);
        let (_, second) = probe.record_success_reporting(7);
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn changed_value_success_reports_change() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        probe.record_success_reporting(7);
        let (_, changed) = probe.record_success_reporting(8);
        assert!(changed);
    }

    #[test]
    fn recovery_success_reports_change_even_with_same_value() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        probe.record_success_reporting(7);
        probe.record_failure_reporting("boom");
        // Stale keeps the last value (7); a fresh success with the same value
        // still changes the rendered state (Stale -> Ok) and clears the error.
        let (_, changed) = probe.record_success_reporting(7);
        assert!(changed);
    }

    #[test]
    fn repeated_identical_failure_reports_no_change() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        let (_, first) = probe.record_failure_reporting("boom");
        let (_, second) = probe.record_failure_reporting("boom");
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn failure_after_success_reports_change() {
        let mut probe = ProbeSnapshot::<u32>::unknown();
        probe.record_success_reporting(7);
        let (_, changed) = probe.record_failure_reporting("timeout");
        assert!(changed);
    }
}
