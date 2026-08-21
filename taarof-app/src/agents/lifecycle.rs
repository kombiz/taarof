//! The one canonical agent lifecycle state machine.
//!
//! Every surface that renders "what is this agent doing" — the native sidebar
//! rows and tab indicators, the agent cards, the task panel, the Unix socket
//! and the HTTP API — resolves through [`resolve`]. There is exactly one place
//! that decides `IDLE` vs `WORKING` vs `WAITING` vs `DONE` vs `ERRORED`.
//!
//! Two vocabularies meet here and must not be confused:
//!
//! - [`crate::workspace::AgentActivityState`] is *evidence*: what one signal
//!   (a socket call, a termprop, an output scan) claims about a pane.
//! - [`AgentLifecycle`] is the *resolved* state: what taarof renders after
//!   weighing every piece of evidence for that pane.
//!
//! # Evidence precedence
//!
//! 1. **Fresh explicit signal** (socket or termprop, younger than
//!    [`crate::workspace::EXPLICIT_ACTIVITY_FRESHNESS`]). The agent reported
//!    its own state through a privileged channel; nothing outranks it.
//! 2. **Fresh attention signal** (any origin, `WaitingInput` or `Errored`).
//!    A permission prompt or a crashed turn is never written to a native
//!    transcript, so the only witness is the terminal. It must outrank an
//!    open turn, or a pane blocked on a y/n prompt would render as WORKING.
//! 3. **Fresh native turn** (younger than [`TRANSCRIPT_TURN_FRESHNESS`]),
//!    folded from the pane's own transcript and stamped with the transcript
//!    record's own timestamp. This is the tier that fixes long thinking
//!    phases: an open turn keeps the pane WORKING even when no Read/Edit/Bash
//!    line has scrolled past for minutes.
//! 4. **Last recorded signal, any age.** `WaitingInput` / `Errored` / `Done`
//!    latch until something replaces them; a `Running` signal only counts
//!    while it is still fresh.
//! 5. **Otherwise `Idle`.** An agent process being present is *detected*, not
//!    *working* — process presence alone never reaches this module.

use crate::workspace::{AgentActivity, AgentActivityState, DONE_ACTIVITY_VISIBILITY};

use std::time::Duration;

/// How long an open native turn keeps a pane in [`AgentLifecycle::Working`]
/// without any further transcript record.
///
/// Sized for thinking, not for typing: a long extended-thinking block or a
/// slow tool call writes nothing to the transcript while it runs, so the old
/// eight-second output-scan window collapsed such a pane to IDLE. It is still
/// bounded, so a transcript written by an unrelated or abandoned session can
/// never hold a quiet pane at WORKING forever.
pub(crate) const TRANSCRIPT_TURN_FRESHNESS: Duration = Duration::from_secs(300);

/// The resolved lifecycle state of one agent pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum AgentLifecycle {
    /// A pane with an agent in it that nothing says is busy — including a
    /// detected-but-quiet agent process.
    #[default]
    Idle,
    /// A turn is open: thinking, calling tools, or streaming a response.
    Working,
    /// Blocked on the human — a prompt, a permission request, a finished turn
    /// that explicitly asked for input.
    WaitingInput,
    /// The agent just finished; a transient state before falling back to idle.
    Done,
    /// The turn ended in a provider-reported failure.
    Errored,
}

impl AgentLifecycle {
    /// Uppercase badge text for the native sidebar and agent cards.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Working => "WORKING",
            Self::WaitingInput => "WAITING",
            Self::Done => "DONE",
            Self::Errored => "ERRORED",
            Self::Idle => "IDLE",
        }
    }

    /// Colourful, glanceable label for native UI badges. Wire/API labels stay
    /// plain and stable through [`Self::label`] and [`Self::wire`].
    pub(crate) fn ui_label(self) -> &'static str {
        match self {
            Self::Working => "🟢 WORKING",
            Self::WaitingInput => "🟡 WAITING",
            Self::Done => "✅ DONE",
            Self::Errored => "🔴 ERRORED",
            Self::Idle => "💤 IDLE",
        }
    }

    /// GTK CSS class driving the state colour.
    pub(crate) fn css_class(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::WaitingInput => "waiting",
            Self::Done => "done",
            Self::Errored => "errored",
            Self::Idle => "idle",
        }
    }

    /// Stable lowercase token for the Unix socket and HTTP API wire shape.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::WaitingInput => "waiting_input",
            Self::Done => "done",
            Self::Errored => "errored",
            Self::Idle => "idle",
        }
    }

    /// True while the agent owns the turn. This is the single definition of
    /// "running" that the sidebar, the API, and the notification path share.
    pub(crate) fn is_working(self) -> bool {
        matches!(self, Self::Working)
    }

    /// True when the pane is blocked on the human or has failed.
    pub(crate) fn needs_attention(self) -> bool {
        matches!(self, Self::WaitingInput | Self::Errored)
    }
}

/// Where a pane's agent sits inside its current turn, folded from that pane's
/// own native transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TurnPhase {
    /// No native turn boundary has been observed for this pane yet.
    #[default]
    Unknown,
    /// A turn is open: a prompt arrived, or the agent is reasoning, calling a
    /// tool, or consuming a tool result.
    Active,
    /// The agent produced a complete response and handed control back.
    Completed,
    /// The provider reported that the turn failed.
    Errored,
}

/// One provider-neutral turn boundary derived from a single native transcript
/// record. Adapters translate their own record vocabulary into these markers;
/// the state machine never sees a provider-specific shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnMarker {
    /// A human prompt opened a new turn.
    Started,
    /// The agent is mid-turn. This covers an assistant record that is still
    /// streaming or that stopped only to call a tool — neither is a completed
    /// response, and neither may close the turn.
    Progress,
    /// The agent produced a complete response and handed control back.
    Completed,
    /// The provider reported that the turn ended in failure.
    Errored,
}

impl TurnMarker {
    pub(crate) fn phase(self) -> TurnPhase {
        match self {
            Self::Started | Self::Progress => TurnPhase::Active,
            Self::Completed => TurnPhase::Completed,
            Self::Errored => TurnPhase::Errored,
        }
    }
}

/// One pane's native turn evidence.
///
/// `at_unix_ms` is the *transcript record's own* timestamp, not the time
/// taarof folded it. That is what keeps replayed history honest: binding to a
/// transcript last written an hour ago yields an hour-old turn, which is stale
/// on arrival and can never make an idle process look active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct PaneTurn {
    pub phase: TurnPhase,
    pub at_unix_ms: u64,
}

impl PaneTurn {
    pub(crate) fn new(phase: TurnPhase, at_unix_ms: u64) -> Self {
        Self { phase, at_unix_ms }
    }

    /// Age of the evidence. Clock skew that puts a record in the future reads
    /// as zero age rather than as a wildly stale turn.
    fn age_ms(self, now_unix_ms: u64) -> u64 {
        now_unix_ms.saturating_sub(self.at_unix_ms)
    }

    fn is_fresh(self, now_unix_ms: u64) -> bool {
        self.age_ms(now_unix_ms) < TRANSCRIPT_TURN_FRESHNESS.as_millis() as u64
    }
}

fn lifecycle_for_signal_state(state: AgentActivityState) -> AgentLifecycle {
    match state {
        AgentActivityState::Idle => AgentLifecycle::Idle,
        AgentActivityState::Running => AgentLifecycle::Working,
        AgentActivityState::WaitingInput => AgentLifecycle::WaitingInput,
        AgentActivityState::Errored => AgentLifecycle::Errored,
        AgentActivityState::Done => AgentLifecycle::Done,
    }
}

/// Resolve one pane's lifecycle state from every piece of evidence taarof has.
///
/// `signal` is the last recorded [`AgentActivity`] for the pane (socket,
/// termprop, or output scan). `turn` is the pane's native transcript turn, when
/// a transcript is bound to it. `now_unix_ms` is injected so the precedence
/// contract is testable without sleeping.
///
/// See the module docs for the precedence contract this implements.
pub(crate) fn resolve(
    signal: Option<&AgentActivity>,
    turn: Option<PaneTurn>,
    now_unix_ms: u64,
) -> AgentLifecycle {
    // 1. The agent reported its own state through a privileged channel.
    if let Some(signal) = signal.filter(|signal| signal.has_fresh_explicit_update()) {
        return lifecycle_for_signal_state(signal.state);
    }

    // 2. A live attention signal. Permission prompts and crashed turns never
    //    reach a native transcript, so the terminal is the only witness.
    if let Some(signal) = signal.filter(|signal| {
        signal.is_fresh()
            && matches!(
                signal.state,
                AgentActivityState::WaitingInput | AgentActivityState::Errored
            )
    }) {
        return lifecycle_for_signal_state(signal.state);
    }

    // 3. Native transcript turn evidence.
    if let Some(turn) = turn.filter(|turn| turn.is_fresh(now_unix_ms)) {
        match turn.phase {
            TurnPhase::Errored => return AgentLifecycle::Errored,
            TurnPhase::Active => return AgentLifecycle::Working,
            TurnPhase::Completed => {
                // A finished turn is only newsworthy briefly; after that the
                // pane is simply idle and waiting for its human.
                if turn.age_ms(now_unix_ms) < DONE_ACTIVITY_VISIBILITY.as_millis() as u64 {
                    return AgentLifecycle::Done;
                }
            }
            TurnPhase::Unknown => {}
        }
    }

    // 4. The last signal recorded for the pane, whatever its age.
    if let Some(signal) = signal {
        match signal.state {
            AgentActivityState::WaitingInput => return AgentLifecycle::WaitingInput,
            AgentActivityState::Errored => return AgentLifecycle::Errored,
            AgentActivityState::Done => return AgentLifecycle::Done,
            AgentActivityState::Running if signal.is_fresh_running_signal() => {
                return AgentLifecycle::Working
            }
            _ => {}
        }
    }

    // 5. Detected, not working.
    AgentLifecycle::Idle
}

/// Resolve the strongest lifecycle across a set of panes, used for tab-level
/// rollups. An error beats a prompt waiting for input, attention beats work,
/// work beats a finished turn, and a finished turn beats idle.
pub(crate) fn strongest(
    lifecycles: impl IntoIterator<Item = AgentLifecycle>,
) -> Option<AgentLifecycle> {
    fn rank(lifecycle: AgentLifecycle) -> u8 {
        match lifecycle {
            AgentLifecycle::Errored => 4,
            AgentLifecycle::WaitingInput => 3,
            AgentLifecycle::Working => 2,
            AgentLifecycle::Done => 1,
            AgentLifecycle::Idle => 0,
        }
    }
    lifecycles.into_iter().max_by_key(|state| rank(*state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::AgentActivity;
    use std::time::Instant;

    const NOW: u64 = 1_800_000_000_000;

    fn aged(mut activity: AgentActivity, age: Duration) -> AgentActivity {
        activity.updated_at = Instant::now() - age;
        activity
    }

    fn socket(state: AgentActivityState, age: Duration) -> AgentActivity {
        aged(
            AgentActivity::socket(state, "signal", Some("claude".into())).expect("socket activity"),
            age,
        )
    }

    fn output_scan(state: AgentActivityState, age: Duration) -> AgentActivity {
        aged(
            AgentActivity::output_scan(state, "signal", Some("claude".into()))
                .expect("output scan activity"),
            age,
        )
    }

    #[test]
    fn an_open_turn_keeps_a_pane_working_far_past_the_output_scan_window() {
        // The whole bug: a stale output-scan Running signal used to decay to
        // IDLE after eight seconds even though the agent was still thinking.
        let signal = output_scan(AgentActivityState::Running, Duration::from_secs(45));
        let turn = PaneTurn::new(TurnPhase::Active, NOW - 40_000);

        assert_eq!(
            resolve(Some(&signal), Some(turn), NOW),
            AgentLifecycle::Working
        );
    }

    #[test]
    fn an_open_turn_alone_is_enough_without_any_terminal_signal() {
        let turn = PaneTurn::new(TurnPhase::Active, NOW - 120_000);

        assert_eq!(resolve(None, Some(turn), NOW), AgentLifecycle::Working);
    }

    #[test]
    fn a_stale_turn_never_holds_a_quiet_pane_at_working() {
        let turn = PaneTurn::new(
            TurnPhase::Active,
            NOW - (TRANSCRIPT_TURN_FRESHNESS.as_millis() as u64 + 1),
        );

        assert_eq!(resolve(None, Some(turn), NOW), AgentLifecycle::Idle);
    }

    #[test]
    fn a_completed_turn_is_briefly_done_and_then_idle() {
        let just_finished = PaneTurn::new(TurnPhase::Completed, NOW - 1_000);
        let long_finished = PaneTurn::new(TurnPhase::Completed, NOW - 60_000);

        assert_eq!(
            resolve(None, Some(just_finished), NOW),
            AgentLifecycle::Done
        );
        assert_eq!(
            resolve(None, Some(long_finished), NOW),
            AgentLifecycle::Idle
        );
    }

    #[test]
    fn a_completed_turn_falls_back_to_a_latched_waiting_signal() {
        // The agent finished and the terminal is showing a prompt: the pane is
        // waiting for its human, not merely idle.
        let signal = output_scan(AgentActivityState::WaitingInput, Duration::from_secs(90));
        let turn = PaneTurn::new(TurnPhase::Completed, NOW - 60_000);

        assert_eq!(
            resolve(Some(&signal), Some(turn), NOW),
            AgentLifecycle::WaitingInput
        );
    }

    #[test]
    fn a_fresh_explicit_signal_outranks_the_transcript() {
        let signal = socket(AgentActivityState::WaitingInput, Duration::from_secs(1));
        let turn = PaneTurn::new(TurnPhase::Active, NOW - 500);

        assert_eq!(
            resolve(Some(&signal), Some(turn), NOW),
            AgentLifecycle::WaitingInput
        );
    }

    #[test]
    fn a_live_permission_prompt_outranks_an_open_turn() {
        // Claude writes nothing to the transcript while it waits on a y/n
        // prompt, so the turn stays open. The terminal is the only witness.
        let signal = output_scan(AgentActivityState::WaitingInput, Duration::from_secs(2));
        let turn = PaneTurn::new(TurnPhase::Active, NOW - 3_000);

        assert_eq!(
            resolve(Some(&signal), Some(turn), NOW),
            AgentLifecycle::WaitingInput
        );
    }

    #[test]
    fn a_live_turn_outranks_a_stale_waiting_signal() {
        // The human answered the prompt eighty seconds ago and the agent is
        // working again; the latched WAITING must not win.
        let signal = output_scan(AgentActivityState::WaitingInput, Duration::from_secs(80));
        let turn = PaneTurn::new(TurnPhase::Active, NOW - 4_000);

        assert_eq!(
            resolve(Some(&signal), Some(turn), NOW),
            AgentLifecycle::Working
        );
    }

    #[test]
    fn an_errored_turn_reports_errored() {
        let turn = PaneTurn::new(TurnPhase::Errored, NOW - 2_000);

        assert_eq!(resolve(None, Some(turn), NOW), AgentLifecycle::Errored);
    }

    #[test]
    fn no_evidence_at_all_is_idle() {
        assert_eq!(resolve(None, None, NOW), AgentLifecycle::Idle);
        assert_eq!(
            resolve(None, Some(PaneTurn::default()), NOW),
            AgentLifecycle::Idle
        );
    }

    #[test]
    fn legacy_signal_only_behaviour_is_unchanged() {
        let fresh_running = output_scan(AgentActivityState::Running, Duration::from_secs(2));
        let stale_running = output_scan(AgentActivityState::Running, Duration::from_secs(30));
        let done = socket(AgentActivityState::Done, Duration::from_secs(60));

        assert_eq!(
            resolve(Some(&fresh_running), None, NOW),
            AgentLifecycle::Working
        );
        assert_eq!(
            resolve(Some(&stale_running), None, NOW),
            AgentLifecycle::Idle
        );
        assert_eq!(resolve(Some(&done), None, NOW), AgentLifecycle::Done);
    }

    #[test]
    fn strongest_ranks_attention_over_work_over_done() {
        assert_eq!(
            strongest([
                AgentLifecycle::Idle,
                AgentLifecycle::Working,
                AgentLifecycle::WaitingInput,
            ]),
            Some(AgentLifecycle::WaitingInput)
        );
        assert_eq!(
            strongest([AgentLifecycle::Done, AgentLifecycle::Working]),
            Some(AgentLifecycle::Working)
        );
        assert_eq!(
            strongest([AgentLifecycle::Idle, AgentLifecycle::Done]),
            Some(AgentLifecycle::Done)
        );
        assert_eq!(strongest([]), None);
    }

    #[test]
    fn strongest_ranks_error_over_waiting_regardless_of_iteration_order() {
        assert_eq!(
            strongest([AgentLifecycle::Errored, AgentLifecycle::WaitingInput,]),
            Some(AgentLifecycle::Errored)
        );
        assert_eq!(
            strongest([AgentLifecycle::WaitingInput, AgentLifecycle::Errored,]),
            Some(AgentLifecycle::Errored)
        );
    }

    #[test]
    fn turn_markers_collapse_to_phases() {
        assert_eq!(TurnMarker::Started.phase(), TurnPhase::Active);
        assert_eq!(TurnMarker::Progress.phase(), TurnPhase::Active);
        assert_eq!(TurnMarker::Completed.phase(), TurnPhase::Completed);
        assert_eq!(TurnMarker::Errored.phase(), TurnPhase::Errored);
    }
}
