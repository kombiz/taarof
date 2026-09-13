//! Byte-bounded lossless native delivery and coalesced replay observer wakeups.
//! Queue waits use only their own mutex; callers must release broker state first.
use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const NATIVE_QUEUE_BYTES: usize = 4 * 1024 * 1024;
pub const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
pub const NATIVE_QUEUE_CHUNKS: usize = NATIVE_QUEUE_BYTES / OUTPUT_CHUNK_BYTES;
pub const MAX_NATIVE_SUBSCRIBERS: usize = 4;
pub const MAX_WEB_OBSERVERS: usize = 16;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct SubscriberStats {
    pub native_subscribers: usize,
    pub native_queued_bytes: usize,
    pub native_queued_chunks: usize,
    pub native_waiting_writers: usize,
    pub web_observers: usize,
}

struct QueueState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    receiver_alive: bool,
    finished: bool,
    cancelled: bool,
    waiting: usize,
}

pub(crate) struct DeliveryQueue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

/// One native consumer. Full queues backpressure the sole reader without loss.
/// EOF drains queued bytes; pane cancellation or receiver drop discards them as
/// explicit teardown, wakes blocked writers and never reports successful drain.
pub struct NativeSubscription {
    queue: Arc<DeliveryQueue>,
}

impl DeliveryQueue {
    pub(crate) fn new() -> (Arc<Self>, NativeSubscription) {
        let queue = Arc::new(Self {
            state: Mutex::new(QueueState {
                chunks: VecDeque::new(),
                bytes: 0,
                receiver_alive: true,
                finished: false,
                cancelled: false,
                waiting: 0,
            }),
            changed: Condvar::new(),
        });
        (queue.clone(), NativeSubscription { queue })
    }

    /// Pack a bounded replay seed before returning the receiver. Never wait for
    /// a consumer that cannot yet run, and do not retain a second seed buffer.
    pub(crate) fn seed<'a>(&self, payloads: impl Iterator<Item = &'a [u8]>) {
        let mut state = self.state.lock().unwrap();
        for mut payload in payloads {
            while !payload.is_empty() {
                if state
                    .chunks
                    .back()
                    .is_none_or(|chunk| chunk.len() == OUTPUT_CHUNK_BYTES)
                {
                    state
                        .chunks
                        .push_back(Vec::with_capacity(OUTPUT_CHUNK_BYTES));
                }
                let count = payload
                    .len()
                    .min(OUTPUT_CHUNK_BYTES - state.chunks.back().unwrap().len());
                state
                    .chunks
                    .back_mut()
                    .unwrap()
                    .extend_from_slice(&payload[..count]);
                state.bytes += count;
                payload = &payload[count..];
            }
        }
        assert!(state.bytes <= NATIVE_QUEUE_BYTES && state.chunks.len() <= NATIVE_QUEUE_CHUNKS);
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> bool {
        assert!(bytes.len() <= OUTPUT_CHUNK_BYTES);
        let mut state = self.state.lock().unwrap();
        while state.receiver_alive
            && !state.cancelled
            && (state.bytes + bytes.len() > NATIVE_QUEUE_BYTES
                || state.chunks.len() >= NATIVE_QUEUE_CHUNKS)
        {
            state.waiting += 1;
            state = self.changed.wait(state).unwrap();
            state.waiting -= 1;
        }
        if !state.receiver_alive || state.cancelled {
            return false;
        }
        state.bytes += bytes.len();
        state.chunks.push_back(bytes.to_vec());
        self.changed.notify_all();
        true
    }

    pub(crate) fn finish(&self) {
        self.state.lock().unwrap().finished = true;
        self.changed.notify_all();
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        state.chunks.clear();
        state.bytes = 0;
        self.changed.notify_all();
    }

    pub(crate) fn stats(&self) -> (bool, usize, usize, usize) {
        let state = self.state.lock().unwrap();
        (
            state.receiver_alive,
            state.bytes,
            state.chunks.len(),
            state.waiting,
        )
    }
}

impl NativeSubscription {
    /// Distinguish explicit pane cancellation from a naturally drained EOF.
    pub fn cancelled(&self) -> bool {
        self.queue.state.lock().unwrap().cancelled
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.queue.state.lock().unwrap();
        loop {
            if state.cancelled {
                return Err(RecvTimeoutError::Disconnected);
            }
            if let Some(chunk) = state.chunks.pop_front() {
                state.bytes -= chunk.len();
                self.queue.changed.notify_all();
                return Ok(chunk);
            }
            if state.finished {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            state = self.queue.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

impl Drop for NativeSubscription {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        state.receiver_alive = false;
        state.chunks.clear();
        state.bytes = 0;
        self.queue.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputWake {
    pub closed: bool,
    pub output_seq: super::OutputSeq,
}

/// Private watch receiver prevents uncounted clones or a held watch borrow from
/// blocking the broker writer. Observers retain a wake bit, never output bytes.
pub struct OutputObserver {
    pub(crate) receiver: tokio::sync::watch::Receiver<OutputWake>,
}
impl OutputObserver {
    pub async fn changed(&mut self) -> bool {
        self.receiver.changed().await.is_ok()
    }
    pub fn closed(&self) -> bool {
        self.receiver.borrow().closed
    }
    pub(crate) fn remaining_after(&self, cursor: super::OutputSeq) -> (bool, bool) {
        let observed = *self.receiver.borrow();
        (observed.closed, cursor < observed.output_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn full_seed_is_nonblocking_and_live_delivery_waits_without_losing_order() {
        let (queue, receiver) = DeliveryQueue::new();
        let seed = vec![b'a'; NATIVE_QUEUE_BYTES];
        queue.seed(std::iter::once(seed.as_slice()));
        let (done, result) = mpsc::channel();
        let writer = queue.clone();
        let thread = std::thread::spawn(move || {
            done.send(writer.send(b"LIVE")).unwrap();
        });
        let until = Instant::now() + Duration::from_secs(2);
        while queue.stats().3 == 0 {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        assert!(result.try_recv().is_err());
        let mut bytes = Vec::new();
        for _ in 0..NATIVE_QUEUE_CHUNKS {
            bytes.extend(receiver.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        assert_eq!(bytes, seed);
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            b"LIVE"
        );
        thread.join().unwrap();
    }
    #[test]
    fn cancellation_and_receiver_drop_release_full_queue_writers() {
        for drop_receiver in [false, true] {
            let (queue, receiver) = DeliveryQueue::new();
            queue.seed(std::iter::once(vec![0; NATIVE_QUEUE_BYTES].as_slice()));
            let writer = queue.clone();
            let (done, result) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                done.send(writer.send(b"blocked")).unwrap();
            });
            let until = Instant::now() + Duration::from_secs(2);
            while queue.stats().3 == 0 {
                assert!(Instant::now() < until);
                std::thread::yield_now();
            }
            if drop_receiver {
                drop(receiver);
            } else {
                queue.cancel();
            }
            assert!(!result.recv_timeout(Duration::from_secs(1)).unwrap());
            assert_eq!(queue.stats().1, 0);
            thread.join().unwrap();
        }
    }
    #[test]
    fn natural_eof_drains_nonempty_queue_in_order_before_disconnect() {
        let (queue, receiver) = DeliveryQueue::new();
        assert!(queue.send(b"first"));
        assert!(queue.send(b"final sentinel"));
        queue.finish();
        assert_eq!(receiver.recv_timeout(Duration::ZERO).unwrap(), b"first");
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO).unwrap(),
            b"final sentinel"
        );
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        );
    }
    #[test]
    fn final_push_after_empty_replay_pass_remains_pending_at_eof() {
        let cursor = super::super::OutputSeq::new(7);
        let (sender, receiver) = tokio::sync::watch::channel(OutputWake {
            closed: false,
            output_seq: cursor,
        });
        let observer = OutputObserver { receiver };
        assert_eq!(observer.remaining_after(cursor), (false, false));
        // The reader commits one final frame and EOF after the sender's last
        // empty replay pass, before it checks source completion.
        sender.send_replace(OutputWake {
            closed: true,
            output_seq: super::super::OutputSeq::new(8),
        });
        assert_eq!(observer.remaining_after(cursor), (true, true));
        assert_eq!(
            observer.remaining_after(super::super::OutputSeq::new(8)),
            (true, false)
        );
    }
}
