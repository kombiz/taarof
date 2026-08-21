pub mod epoch;
pub mod replay;
pub mod screen;
pub mod unix_pty;

pub use epoch::{BrokerEpoch, BrokerEpochParseError, OutputSeq, OutputSeqOverflow};
pub use replay::{ReplayFrame, ReplayWindow, ResumeDecision};
pub use screen::{CanonicalCheckpoint, TerminalProjection, TerminalStateModel};
pub use unix_pty::{BrokeredPane, PtyBroker, SpawnSpec};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn ts(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn epoch() -> BrokerEpoch {
        BrokerEpoch::from_bytes([0x11; 16])
    }

    #[test]
    fn output_sequences_allocate_monotonically() {
        let mut seq = OutputSeq::zero();

        let first = seq.allocate_next().expect("first sequence allocates");
        let second = seq.allocate_next().expect("second sequence allocates");

        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(seq.get(), 2);
    }

    #[test]
    fn output_sequence_overflow_is_explicit() {
        let mut seq = OutputSeq::new(u64::MAX);

        let err = seq
            .allocate_next()
            .expect_err("u64::MAX cannot allocate another output sequence");

        assert_eq!(err, OutputSeqOverflow);
        assert_eq!(seq.get(), u64::MAX);
    }

    #[test]
    fn broker_epoch_uses_canonical_uuid_wire_form() {
        let parsed: BrokerEpoch = "6d43882a-7f60-40bb-9f6d-166f7b63d10d"
            .parse()
            .expect("valid uuid parses");

        assert_eq!(parsed.to_string(), "6d43882a-7f60-40bb-9f6d-166f7b63d10d");
        assert!("6D43882A-7F60-40BB-9F6D-166F7B63D10D"
            .parse::<BrokerEpoch>()
            .is_err());
        assert!("not-a-uuid".parse::<BrokerEpoch>().is_err());
    }

    #[test]
    fn replay_window_evicts_complete_oldest_frames_by_bytes() {
        let mut window = ReplayWindow::with_limits(epoch(), 10, Duration::from_secs(30));

        window.push(ts(1), b"12345").expect("first frame");
        window.push(ts(2), b"abcd").expect("second frame");
        window
            .push(ts(3), b"xyz")
            .expect("third frame evicts first");

        assert_eq!(window.total_bytes(), 7);
        assert_eq!(window.oldest_retained_seq(), Some(OutputSeq::new(2)));
        assert_eq!(window.latest_seq(), OutputSeq::new(3));
        assert_eq!(
            window
                .frames()
                .iter()
                .map(|frame| frame.payload.as_slice())
                .collect::<Vec<_>>(),
            vec![b"abcd".as_slice(), b"xyz".as_slice()]
        );
    }

    #[test]
    fn replay_window_evicts_complete_oldest_frames_by_age() {
        let mut window = ReplayWindow::with_limits(epoch(), 1024, Duration::from_secs(30));

        window.push(ts(10), b"old").expect("old frame");
        window.push(ts(39), b"edge").expect("edge frame");
        window.push(ts(41), b"new").expect("new frame");

        assert_eq!(window.oldest_retained_seq(), Some(OutputSeq::new(2)));
        assert_eq!(
            window
                .frames()
                .iter()
                .map(|frame| frame.payload.as_slice())
                .collect::<Vec<_>>(),
            vec![b"edge".as_slice(), b"new".as_slice()]
        );
    }

    #[test]
    fn exact_resume_returns_ordered_bytes_after_cursor() {
        let epoch = epoch();
        let mut window = ReplayWindow::with_limits(epoch, 1024, Duration::from_secs(30));
        window.push(ts(1), b"one").expect("one");
        window.push(ts(2), b"two").expect("two");
        window.push(ts(3), b"\xff\xfe").expect("raw bytes");

        let decision = window.resume(epoch, OutputSeq::new(1));

        assert_eq!(
            decision,
            ResumeDecision::Replay {
                epoch,
                ack_seq: OutputSeq::new(1),
                frames: vec![
                    ReplayFrame::new(OutputSeq::new(2), ts(2), b"two".to_vec()),
                    ReplayFrame::new(OutputSeq::new(3), ts(3), b"\xff\xfe".to_vec()),
                ],
            }
        );
    }

    #[test]
    fn cursor_at_latest_is_explicit_for_empty_and_non_empty_windows() {
        let epoch = epoch();
        let empty = ReplayWindow::with_limits(epoch, 1024, Duration::from_secs(30));
        assert_eq!(
            empty.resume(epoch, OutputSeq::zero()),
            ResumeDecision::AlreadyAtLatest {
                epoch,
                latest_seq: OutputSeq::zero(),
            }
        );

        let mut window = ReplayWindow::with_limits(epoch, 1024, Duration::from_secs(30));
        window.push(ts(1), b"one").expect("one");

        assert_eq!(
            window.resume(epoch, OutputSeq::new(1)),
            ResumeDecision::AlreadyAtLatest {
                epoch,
                latest_seq: OutputSeq::new(1),
            }
        );
    }

    #[test]
    fn resume_gap_wrong_epoch_and_future_cursor_are_distinct() {
        let epoch = epoch();
        let other_epoch = BrokerEpoch::from_bytes([0x22; 16]);
        let mut window = ReplayWindow::with_limits(epoch, 10, Duration::from_secs(30));
        window.push(ts(1), b"12345").expect("first frame");
        window.push(ts(2), b"abcd").expect("second frame");
        window
            .push(ts(3), b"xyz")
            .expect("third frame evicts first");

        assert_eq!(
            window.resume(epoch, OutputSeq::zero()),
            ResumeDecision::ReplayGap {
                epoch,
                requested_seq: OutputSeq::zero(),
                oldest_retained_seq: Some(OutputSeq::new(2)),
                latest_seq: OutputSeq::new(3),
            }
        );
        assert_eq!(
            window.resume(other_epoch, OutputSeq::new(2)),
            ResumeDecision::WrongEpoch {
                requested_epoch: other_epoch,
                current_epoch: epoch,
            }
        );
        assert_eq!(
            window.resume(epoch, OutputSeq::new(4)),
            ResumeDecision::FutureCursor {
                epoch,
                requested_seq: OutputSeq::new(4),
                latest_seq: OutputSeq::new(3),
            }
        );
    }

    #[test]
    fn replay_window_preserves_limits_when_a_single_frame_is_too_large() {
        let epoch = epoch();
        let mut window = ReplayWindow::with_limits(epoch, 4, Duration::from_secs(30));

        window
            .push(ts(1), b"12345")
            .expect("oversized frame is accepted but not retained");

        assert_eq!(window.total_bytes(), 0);
        assert_eq!(window.oldest_retained_seq(), None);
        assert_eq!(window.latest_seq(), OutputSeq::new(1));
        assert_eq!(
            window.resume(epoch, OutputSeq::zero()),
            ResumeDecision::ReplayGap {
                epoch,
                requested_seq: OutputSeq::zero(),
                oldest_retained_seq: None,
                latest_seq: OutputSeq::new(1),
            }
        );
    }

    #[test]
    fn replay_window_preserves_age_invariant_with_non_monotonic_timestamps() {
        let epoch = epoch();
        let mut window = ReplayWindow::with_limits(epoch, 1024, Duration::from_secs(30));

        window.push(ts(100), b"newer").expect("newer frame");
        window.push(ts(60), b"older").expect("older timestamp");
        window.push(ts(101), b"latest").expect("latest frame");

        assert_eq!(window.total_bytes(), 6);
        assert_eq!(
            window
                .frames()
                .iter()
                .map(|frame| frame.payload.as_slice())
                .collect::<Vec<_>>(),
            vec![b"latest".as_slice()]
        );
        assert_eq!(window.oldest_retained_seq(), Some(OutputSeq::new(3)));
        assert_eq!(window.latest_seq(), OutputSeq::new(3));
    }
}
