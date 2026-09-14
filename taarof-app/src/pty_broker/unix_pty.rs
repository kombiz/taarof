//! Broker-owned Unix pseudoterminal.
//!
//! The broker allocates the PTY explicitly, spawns the child on the slave, and
//! becomes the **sole reader** of the master. Every chunk read from the master
//! feeds bounded replay and the terminal model under one lock, then delivers
//! ordered bytes to bounded native queues outside that lock. Slow native
//! consumers backpressure the sole reader; WebSocket observers coalesce wakes
//! and recover from the same replay/checkpoint authority.
//!
//! This module is intentionally free of GTK/VTE dependencies so it runs under
//! the headless test suite. The glue that turns a subscriber channel into
//! `vte::Terminal::feed` calls, and that routes VTE input/resize back into
//! [`BrokeredPane::write_input`]/[`BrokeredPane::resize`], lives in the
//! terminal/pane modules.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Weak;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use super::epoch::{BrokerEpoch, OutputSeq};
use super::replay::ReplayWindow;
use super::screen::{CanonicalCheckpoint, TerminalStateModel};

use super::output::{DeliveryQueue, OutputWake};
use super::{
    NativeSubscription, OutputObserver, SubscriberStats, MAX_NATIVE_SUBSCRIBERS, MAX_WEB_OBSERVERS,
    OUTPUT_CHUNK_BYTES,
};
const READ_CHUNK_BYTES: usize = OUTPUT_CHUNK_BYTES;

/// Everything required to spawn a child under a broker-owned PTY.
pub struct SpawnSpec {
    /// Full argv; `argv[0]` is the program to execute.
    pub argv: Vec<String>,
    /// Working directory for the child, or its inherited default when `None`.
    pub cwd: Option<PathBuf>,
    /// Extra environment overrides applied on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// Namespace for broker construction. `PtyBroker::spawn` allocates the PTY and
/// returns the live [`BrokeredPane`].
pub struct PtyBroker;

impl PtyBroker {
    /// Allocate a PTY, spawn the child on its slave, and start the sole reader.
    pub fn spawn(spec: SpawnSpec) -> io::Result<BrokeredPane> {
        BrokeredPane::spawn(spec, false).map(|(pane, _)| pane)
    }
    /// Register native delivery before the reader starts, so even a fast child
    /// cannot outrun the initial presentation subscription.
    pub fn spawn_with_native(spec: SpawnSpec) -> io::Result<(BrokeredPane, NativeSubscription)> {
        BrokeredPane::spawn(spec, true).map(|(pane, receiver)| (pane, receiver.unwrap()))
    }
}

struct BrokerState {
    replay: ReplayWindow,
    model: TerminalStateModel,
    subscribers: Vec<Weak<DeliveryQueue>>,
    output_wake: tokio::sync::watch::Sender<OutputWake>,
    closed: bool,
    output_error: Option<i32>,
}

/// A live pane whose PTY the broker owns. Cloning is intentionally not
/// supported: exactly one owner drives child lifecycle and the reader thread.
pub struct BrokeredPane {
    epoch: BrokerEpoch,
    master: Arc<OwnedFd>,
    input: super::input::InputWriter,
    shared: Arc<Mutex<BrokerState>>,
    child: Mutex<Child>,
    child_pid: u32,
    reader: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    #[cfg(test)]
    checkpoint_sampled: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl BrokeredPane {
    fn spawn(spec: SpawnSpec, native: bool) -> io::Result<(Self, Option<NativeSubscription>)> {
        if spec.argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SpawnSpec.argv must contain at least the program to execute",
            ));
        }

        let (master, slave) = allocate_pty()?;
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        set_winsize(master.as_raw_fd(), spec.cols, spec.rows)?;

        let child = spawn_child(&spec, master.as_raw_fd(), slave.as_raw_fd())?;
        // The parent must drop its slave handle: while any process holds the
        // slave open the master read never reaches EOF, so the reader thread
        // could never observe the child exiting.
        drop(slave);
        let child_pid = child.id();

        let epoch = BrokerEpoch::new()
            .map_err(|err| io::Error::other(format!("broker epoch entropy failed: {err}")))?;
        let (output_wake, _) = tokio::sync::watch::channel(OutputWake {
            closed: false,
            output_seq: OutputSeq::zero(),
        });
        let native = native.then(DeliveryQueue::new);
        let subscribers = native
            .as_ref()
            .map(|(queue, _)| vec![Arc::downgrade(queue)])
            .unwrap_or_default();
        let shared = Arc::new(Mutex::new(BrokerState {
            replay: ReplayWindow::new(epoch),
            model: TerminalStateModel::new(spec.cols as usize, spec.rows as usize),
            subscribers,
            output_wake,
            closed: false,
            output_error: None,
        }));
        let master = Arc::new(master);

        let input = super::input::InputWriter::new(Arc::clone(&master));
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader(Arc::clone(&master), Arc::clone(&shared), shutdown.clone());

        Ok((
            Self {
                epoch,
                master,
                input,
                shared,
                child: Mutex::new(child),
                child_pid,
                reader: Some(reader),
                shutdown,
                #[cfg(test)]
                checkpoint_sampled: Mutex::new(None),
            },
            native.map(|(_, receiver)| receiver),
        ))
    }

    /// The connection epoch assigned to this pane's output stream.
    pub fn epoch(&self) -> BrokerEpoch {
        self.epoch
    }

    /// The spawned child's process id.
    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    /// Register a bounded native subscriber. Late attachment is lossless only
    /// while replay still contains the complete stream; otherwise fail explicitly.
    /// The product uses spawn_with_native to avoid this late-attachment race.
    pub fn subscribe(&self) -> io::Result<NativeSubscription> {
        let mut state = self.shared.lock().expect("broker state lock poisoned");
        state
            .subscribers
            .retain(|entry| entry.upgrade().is_some_and(|queue| queue.stats().0));
        if state.subscribers.len() >= MAX_NATIVE_SUBSCRIBERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "native subscriber limit reached",
            ));
        }
        if state.replay.latest_seq().get() > 0
            && state.replay.oldest_retained_seq() != Some(OutputSeq::new(1))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native output history was evicted; cannot attach without losing bytes",
            ));
        }
        let (queue, receiver) = DeliveryQueue::new();
        queue.seed(
            state
                .replay
                .frames()
                .iter()
                .map(|frame| frame.payload.as_slice()),
        );
        if state.closed {
            queue.finish();
        }
        state.subscribers.push(Arc::downgrade(&queue));
        Ok(receiver)
    }

    /// Coalesced output/EOF observation. Registration and its count check are
    /// atomic, and callers cannot clone the receiver to bypass admission.
    pub fn observe_output(&self) -> io::Result<OutputObserver> {
        let state = self.shared.lock().expect("broker state lock poisoned");
        if state.output_wake.receiver_count() >= MAX_WEB_OBSERVERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "web observer limit reached",
            ));
        }
        Ok(OutputObserver {
            receiver: state.output_wake.subscribe(),
        })
    }

    pub fn subscriber_stats(&self) -> SubscriberStats {
        let (queues, web_observers) = {
            let state = self.shared.lock().expect("broker state lock poisoned");
            (
                state
                    .subscribers
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>(),
                state.output_wake.receiver_count(),
            )
        };
        let mut stats = SubscriberStats {
            web_observers,
            ..SubscriberStats::default()
        };
        for queue in queues {
            let (alive, bytes, chunks, waiting) = queue.stats();
            if alive {
                stats.native_subscribers += 1;
            }
            stats.native_queued_bytes += bytes;
            stats.native_queued_chunks += chunks;
            stats.native_waiting_writers += waiting;
        }
        stats
    }

    /// Admit bounded input without performing I/O on the caller. A receipt
    /// reports kernel-accepted byte counts; dropping it cancels the remainder.
    pub fn submit_input(
        &self,
        bytes: Vec<u8>,
        deadline: std::time::Instant,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<super::InputReceipt> {
        self.input.submit(bytes, Some(deadline), cancelled, false)
    }

    /// Synchronous off-thread native input. Uses the same ordered writer with
    /// backpressure, preserving VTE's encoded bytes until delivered or pane close.
    /// GTK and async request handlers must use submit_input instead.
    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        let receipt = self.input.submit(
            bytes.to_vec(),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            true,
        )?;
        let outcome = receipt.wait_blocking();
        if outcome.status == super::InputStatus::Delivered {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!(
                    "PTY input {:?}: {} of {} bytes accepted",
                    outcome.status, outcome.written_bytes, outcome.requested_bytes
                ),
            ))
        }
    }

    /// Resize the PTY and the headless state model to `cols` x `rows`.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        set_winsize(self.master.as_raw_fd(), cols, rows)?;
        let mut state = self.shared.lock().expect("broker state lock poisoned");
        state.model.resize(cols as usize, rows as usize);
        Ok(())
    }

    pub fn subscriber_count(&self) -> usize {
        let stats = self.subscriber_stats();
        stats.native_subscribers + stats.web_observers
    }

    /// Inspect the bounded replay window under the broker lock.
    pub fn with_replay<R>(&self, f: impl FnOnce(&ReplayWindow) -> R) -> R {
        let state = self.shared.lock().expect("broker state lock poisoned");
        f(&state.replay)
    }

    /// Inspect the headless terminal-state model under the broker lock.
    pub fn with_model<R>(&self, f: impl FnOnce(&TerminalStateModel) -> R) -> R {
        let state = self.shared.lock().expect("broker state lock poisoned");
        f(&state.model)
    }

    /// Bound subscriber checkpoint copies before serializing model state.
    pub fn bounded_checkpoint(
        &self,
        max_bytes: usize,
    ) -> io::Result<(OutputSeq, CanonicalCheckpoint)> {
        let state = self.shared.lock().expect("broker state lock poisoned");
        if state.model.has_degraded_cells() {
            return Err(super::screen::ModelDegraded.into());
        }
        if !state.model.checkpoint_fits(max_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal checkpoint exceeds observer memory limit",
            ));
        }
        let checkpoint = state.model.checkpoint()?;
        debug_assert!(checkpoint.ansi.len() <= max_bytes);
        Ok((state.replay.latest_seq(), checkpoint))
    }

    /// Capture the replay cursor and the screen it describes as one observation.
    pub fn checkpoint(&self) -> io::Result<(OutputSeq, CanonicalCheckpoint)> {
        let state = self.shared.lock().expect("broker state lock poisoned");
        let latest = state.replay.latest_seq();
        #[cfg(test)]
        if let Some(sampled) = self.checkpoint_sampled.lock().unwrap().take() {
            sampled();
        }
        Ok((latest, state.model.checkpoint()?))
    }

    /// Wait for the child to exit and reap it.
    pub fn wait(&self) -> io::Result<ExitStatus> {
        self.child
            .lock()
            .expect("broker child lock poisoned")
            .wait()
    }

    /// Terminal reader fault, excluding normal Linux PTY EIO/EOF.
    pub fn output_error(&self) -> Option<i32> {
        self.shared
            .lock()
            .expect("broker state lock poisoned")
            .output_error
    }

    /// The child's exit code if it has already exited, without blocking. Returns
    /// `None` while the child is still running. A child killed by a signal has no
    /// exit code, so it is reported as `128 + signal` (the shell convention),
    /// preserving failure so callers do not read a crash as success.
    pub fn try_exit_code(&self) -> Option<i32> {
        let mut child = self.child.lock().ok()?;
        child.try_wait().ok()?.map(exit_status_code)
    }

    /// Kill and reap the child. Idempotent: safe to call from an explicit
    /// teardown and again from `Drop`. Killing the child closes its slave, so
    /// the reader's poll wakes and its next read returns EOF/EIO so it exits.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let queues = {
            let mut state = self.shared.lock().expect("broker state lock poisoned");
            state.closed = true;
            state.output_wake.send_replace(OutputWake {
                closed: true,
                output_seq: state.replay.latest_seq(),
            });
            state
                .subscribers
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for queue in queues {
            queue.cancel();
        }
        self.input.shutdown();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for BrokeredPane {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Spawn the sole-reader thread. Each `read` result is fanned out to replay,
/// the model, and the subscriber membership snapshot under one lock. Native
/// delivery then waits outside that lock, preserving byte order without blocking
/// broker queries or cancellation.
fn spawn_reader(
    master: Arc<OwnedFd>,
    shared: Arc<Mutex<BrokerState>>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let fd = master.as_raw_fd();
        let mut buf = [0u8; READ_CHUNK_BYTES];
        let mut output_error = None;
        while !shutdown.load(Ordering::Acquire) {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 200) };
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                output_error = Some(
                    io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                );
                break;
            }
            let read =
                unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
            if read > 0 {
                let chunk = &buf[..read as usize];
                let subscribers = {
                    let mut state = shared.lock().expect("broker state lock poisoned");
                    if state.closed {
                        break;
                    }
                    let _ = state.replay.push(SystemTime::now(), chunk);
                    state.model.feed(chunk);
                    state
                        .subscribers
                        .retain(|entry| entry.upgrade().is_some_and(|queue| queue.stats().0));
                    state.output_wake.send_replace(OutputWake {
                        closed: false,
                        output_seq: state.replay.latest_seq(),
                    });
                    state
                        .subscribers
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>()
                };
                // The sole reader cannot read the next chunk until every native
                // delivery accepts this one. Its capacity wait holds no broker
                // state lock: GTK queries, resize, checkpoints and close proceed.
                for subscriber in subscribers {
                    subscriber.send(chunk);
                }
                continue;
            }
            if read == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            // EIO on Linux is the master-side signal that the slave closed, i.e.
            // the child exited. Treat any other error as a terminal read fault.
            if err.raw_os_error() != Some(libc::EIO) {
                output_error = Some(err.raw_os_error().unwrap_or(libc::EIO));
            }
            break;
        }

        if let Some(code) = output_error {
            eprintln!("taarof: PTY reader failed with OS error {code}");
        }
        let mut state = shared.lock().expect("broker state lock poisoned");
        state.output_error = output_error;
        state.closed = true;
        state.output_wake.send_replace(OutputWake {
            closed: true,
            output_seq: state.replay.latest_seq(),
        });
        for queue in state.subscribers.iter().filter_map(Weak::upgrade) {
            queue.finish();
        }
    })
}

/// Allocate a Linux PTY pair with close-on-exec set at creation, before any
/// concurrent helper can exec. TIOCGPTPEER opens this master's slave directly,
/// without a shared ptsname buffer or a path lookup race. OwnedFd releases the
/// master on any subsequent setup failure.
pub(crate) fn allocate_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    let master_raw = unsafe { libc::posix_openpt(flags) };
    if master_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = non_stdio_fd(unsafe { OwnedFd::from_raw_fd(master_raw) })?;
    let master_raw = master.as_raw_fd();
    if unsafe { libc::grantpt(master_raw) } < 0 || unsafe { libc::unlockpt(master_raw) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave_raw = unsafe { libc::ioctl(master_raw, libc::TIOCGPTPEER, flags) };
    if slave_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = non_stdio_fd(unsafe { OwnedFd::from_raw_fd(slave_raw) })?;
    Ok((master, slave))
}

/// Keep both handles above stdio even if the launcher closed 0, 1, or 2.
/// Otherwise pre_exec's dup2/close sequence can overwrite a source descriptor
/// or leave FD_CLOEXEC set when dup2's source equals its destination.
fn non_stdio_fd(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        return Ok(fd);
    }
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Spawn the child on the PTY slave: a new session with the slave as its
/// controlling terminal and stdio, with the parent's master closed in the child.
fn spawn_child(spec: &SpawnSpec, master_raw: RawFd, slave_raw: RawFd) -> io::Result<Child> {
    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    if let Some(cwd) = spec.cwd.as_ref() {
        command.current_dir(cwd);
    }
    if !spec.env.iter().any(|(key, _)| key == "TERM") {
        command.env("TERM", "xterm-256color");
    }
    crate::child_env::prepare_child_command(&mut command, &spec.env);

    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave_raw, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            for target in 0..3 {
                if libc::dup2(slave_raw, target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slave_raw > 2 {
                libc::close(slave_raw);
            }
            libc::close(master_raw);
            Ok(())
        });
    }

    // The broker stores this Child and reaps it via wait()/try_wait(), including
    // the kill-then-wait teardown path.
    #[allow(clippy::disallowed_methods)]
    command.spawn()
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Normalize a child exit status to an integer code. A normal exit uses its
/// code; a signal-terminated child has no code, so it maps to `128 + signal`
/// (the shell convention), keeping a crash/kill a non-zero failure rather than
/// silently reading as success.
fn exit_status_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(0)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::super::ResumeDecision;
    use super::*;
    use std::sync::{Barrier, TryLockError};

    fn apply(state: &mut BrokerState, bytes: &[u8]) {
        state.replay.push(SystemTime::now(), bytes).unwrap();
        state.model.feed(bytes);
    }

    #[test]
    fn oversized_combining_and_resized_checkpoints_fail_before_copying() {
        let pane = PtyBroker::spawn(SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "read -r hold".into()],
            cwd: None,
            env: vec![],
            cols: 4,
            rows: 2,
        })
        .unwrap();
        {
            let mut state = pane.shared.lock().unwrap();
            apply(
                &mut state,
                format!("a{}", "\u{301}".repeat(70 * 1024)).as_bytes(),
            );
        }
        let cursor = pane.with_replay(|window| window.latest_seq());
        assert!(pane.bounded_checkpoint(256 * 1024).is_err());
        assert_eq!(pane.with_replay(|window| window.latest_seq()), cursor);
        // Output is exact in replay; model truncation is explicit and bounded.
        assert!(pane.with_model(|model| model.projection().is_err()));
        assert!(
            pane.with_model(|model| model.retained_text_bytes().unwrap())
                <= 16 * super::super::screen::CELL_TEXT_MAX_BYTES
        );
        pane.resize(512, 512).unwrap();
        assert!(pane.bounded_checkpoint(256 * 1024).is_err());
        pane.shutdown();
    }

    #[test]
    fn checkpoint_cursor_excludes_output_forced_between_sampling_and_serialization() {
        for (initial, next, replay_bytes) in [
            (b"A".as_slice(), b"B".as_slice(), 1024),
            (b"AB".as_slice(), b"\x1b[1DC".as_slice(), 1024),
            (b"ABC".as_slice(), b"\x1b[1D\x1b[KZ".as_slice(), 1024),
            (b"A".as_slice(), b"\x1b[?1049hB".as_slice(), 1024),
            // An oversized initial frame leaves a real replay gap. Recovery
            // must still pair the checkpoint with its exact output cursor.
            (b"AB".as_slice(), b"C".as_slice(), 1),
        ] {
            let pane = Arc::new(
                PtyBroker::spawn(SpawnSpec {
                    argv: vec!["/bin/sh".into(), "-c".into(), "read -r hold".into()],
                    cwd: None,
                    env: Vec::new(),
                    cols: 80,
                    rows: 24,
                })
                .unwrap(),
            );
            {
                let mut state = pane.shared.lock().unwrap();
                state.replay = ReplayWindow::with_limits(
                    pane.epoch(),
                    replay_bytes,
                    std::time::Duration::from_secs(60),
                );
                apply(&mut state, initial);
            }
            if replay_bytes == 1 {
                assert!(matches!(
                    pane.with_replay(|window| window.resume(pane.epoch(), OutputSeq::zero())),
                    ResumeDecision::ReplayGap { .. }
                ));
            }
            let sampled = Arc::new(Barrier::new(2));
            let attempted = Arc::new(Barrier::new(2));
            *pane.checkpoint_sampled.lock().unwrap() = Some(Box::new({
                let sampled = sampled.clone();
                let attempted = attempted.clone();
                move || {
                    sampled.wait();
                    attempted.wait();
                }
            }));
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    sampled.wait();
                    // Force the writer into exactly the former sampling gap.
                    // With one lock it cannot apply until capture returns.
                    match pane.shared.try_lock() {
                        Ok(mut state) => {
                            apply(&mut state, next);
                            attempted.wait();
                        }
                        Err(TryLockError::WouldBlock) => {
                            attempted.wait();
                            apply(&mut pane.shared.lock().unwrap(), next);
                        }
                        Err(TryLockError::Poisoned(_)) => panic!("broker lock poisoned"),
                    }
                });
                let (cursor, checkpoint) = pane.checkpoint().unwrap();
                writer.join().unwrap();
                let mut reconstructed = TerminalStateModel::new(80, 24);
                reconstructed.feed(&checkpoint.ansi);
                let decision = pane.with_replay(|window| window.resume(pane.epoch(), cursor));
                let ResumeDecision::Replay { frames, .. } = decision else {
                    panic!("output after checkpoint must replay");
                };
                for frame in &frames {
                    reconstructed.feed(&frame.payload);
                }
                let expected = pane.with_model(|model| model.projection().unwrap());
                assert_eq!(
                    reconstructed.projection().unwrap(),
                    expected,
                    "checkpoint plus replay applied output more than once"
                );
                assert_eq!(cursor, OutputSeq::new(1));
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].seq, OutputSeq::new(2));

                // A subsequent reconnect captures both chunks and resumes at
                // latest, without replaying either one a second time.
                let (cursor, checkpoint) = pane.checkpoint().unwrap();
                let mut reconnected = TerminalStateModel::new(80, 24);
                reconnected.feed(&checkpoint.ansi);
                assert_eq!(reconnected.projection().unwrap(), expected);
                assert!(matches!(
                    pane.with_replay(|window| window.resume(pane.epoch(), cursor)),
                    ResumeDecision::AlreadyAtLatest { .. }
                ));
            });
        }
    }
}
