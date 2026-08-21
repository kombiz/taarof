use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use super::epoch::{BrokerEpoch, OutputSeq, OutputSeqOverflow};

pub const DEFAULT_REPLAY_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_REPLAY_MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayFrame {
    pub seq: OutputSeq,
    pub timestamp: SystemTime,
    pub payload: Vec<u8>,
}

impl ReplayFrame {
    pub fn new(seq: OutputSeq, timestamp: SystemTime, payload: Vec<u8>) -> Self {
        Self {
            seq,
            timestamp,
            payload,
        }
    }

    pub fn len(&self) -> usize {
        self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    Replay {
        epoch: BrokerEpoch,
        ack_seq: OutputSeq,
        frames: Vec<ReplayFrame>,
    },
    AlreadyAtLatest {
        epoch: BrokerEpoch,
        latest_seq: OutputSeq,
    },
    ReplayGap {
        epoch: BrokerEpoch,
        requested_seq: OutputSeq,
        oldest_retained_seq: Option<OutputSeq>,
        latest_seq: OutputSeq,
    },
    WrongEpoch {
        requested_epoch: BrokerEpoch,
        current_epoch: BrokerEpoch,
    },
    FutureCursor {
        epoch: BrokerEpoch,
        requested_seq: OutputSeq,
        latest_seq: OutputSeq,
    },
}

#[derive(Clone, Debug)]
pub struct ReplayWindow {
    epoch: BrokerEpoch,
    max_bytes: usize,
    max_age: Duration,
    latest_seq: OutputSeq,
    latest_timestamp: Option<SystemTime>,
    total_bytes: usize,
    frames: VecDeque<ReplayFrame>,
}

impl ReplayWindow {
    pub fn new(epoch: BrokerEpoch) -> Self {
        Self::with_limits(epoch, DEFAULT_REPLAY_MAX_BYTES, DEFAULT_REPLAY_MAX_AGE)
    }

    pub fn with_limits(epoch: BrokerEpoch, max_bytes: usize, max_age: Duration) -> Self {
        Self {
            epoch,
            max_bytes,
            max_age,
            latest_seq: OutputSeq::zero(),
            latest_timestamp: None,
            total_bytes: 0,
            frames: VecDeque::new(),
        }
    }

    pub fn epoch(&self) -> BrokerEpoch {
        self.epoch
    }

    pub fn latest_seq(&self) -> OutputSeq {
        self.latest_seq
    }

    pub fn oldest_retained_seq(&self) -> Option<OutputSeq> {
        self.frames.front().map(|frame| frame.seq)
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn frames(&self) -> &VecDeque<ReplayFrame> {
        &self.frames
    }

    pub fn push(
        &mut self,
        timestamp: SystemTime,
        payload: impl Into<Vec<u8>>,
    ) -> Result<OutputSeq, OutputSeqOverflow> {
        let seq = self.latest_seq.allocate_next()?;
        let payload = payload.into();
        self.total_bytes = self.total_bytes.saturating_add(payload.len());
        self.frames
            .push_back(ReplayFrame::new(seq, timestamp, payload));
        self.latest_timestamp = Some(match self.latest_timestamp {
            Some(latest) => latest.max(timestamp),
            None => timestamp,
        });
        self.enforce_limits();
        Ok(seq)
    }

    pub fn resume(&self, epoch: BrokerEpoch, ack_seq: OutputSeq) -> ResumeDecision {
        if epoch != self.epoch {
            return ResumeDecision::WrongEpoch {
                requested_epoch: epoch,
                current_epoch: self.epoch,
            };
        }

        if ack_seq > self.latest_seq {
            return ResumeDecision::FutureCursor {
                epoch: self.epoch,
                requested_seq: ack_seq,
                latest_seq: self.latest_seq,
            };
        }

        if ack_seq == self.latest_seq {
            return ResumeDecision::AlreadyAtLatest {
                epoch: self.epoch,
                latest_seq: self.latest_seq,
            };
        }

        let Some(oldest_retained_seq) = self.oldest_retained_seq() else {
            return ResumeDecision::ReplayGap {
                epoch: self.epoch,
                requested_seq: ack_seq,
                oldest_retained_seq: None,
                latest_seq: self.latest_seq,
            };
        };

        let first_replayable_ack = oldest_retained_seq
            .get()
            .checked_sub(1)
            .map(OutputSeq::new)
            .unwrap_or(OutputSeq::zero());

        if ack_seq < first_replayable_ack {
            return ResumeDecision::ReplayGap {
                epoch: self.epoch,
                requested_seq: ack_seq,
                oldest_retained_seq: Some(oldest_retained_seq),
                latest_seq: self.latest_seq,
            };
        }

        ResumeDecision::Replay {
            epoch: self.epoch,
            ack_seq,
            frames: self
                .frames
                .iter()
                .filter(|frame| frame.seq > ack_seq)
                .cloned()
                .collect(),
        }
    }

    fn enforce_limits(&mut self) {
        while self.total_bytes > self.max_bytes {
            self.pop_oldest();
        }

        let Some(latest_timestamp) = self.latest_timestamp else {
            return;
        };

        while let Some(evict_through_idx) = self.frames.iter().position(|frame| {
            latest_timestamp
                .duration_since(frame.timestamp)
                .is_ok_and(|age| age > self.max_age)
        }) {
            for _ in 0..=evict_through_idx {
                self.pop_oldest();
            }
        }
    }

    fn pop_oldest(&mut self) {
        if let Some(frame) = self.frames.pop_front() {
            self.total_bytes = self.total_bytes.saturating_sub(frame.len());
        }
    }
}
