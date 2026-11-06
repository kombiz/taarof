//! One long-lived `taarof-remote stream` per remote host (slice 2 of #96).
//!
//! Each wanted host gets one supervisor thread. The supervisor owns at most
//! one `ssh -T` child at a time. A reader thread decodes that child's NDJSON
//! into the host's shared state. Nothing here touches GTK: the agent-session
//! probe and the host-status poll read the state without I/O.
//!
//! A host without the helper, with another protocol version, or with a stream
//! that keeps failing falls back to the shell probe for a while. The supervisor
//! then tries the stream again, so a later install takes effect on its own.

use agent_session_core::discovery::unix_time_ms;
use agent_session_core::remote_stream::{
    Event, Metrics, Stats, MAX_EVENT_LINE_BYTES, PROTOCOL_VERSION,
};
use agent_session_core::AgentSessionRecord;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The remote command. Non-interactive SSH often lacks mise's shims on PATH,
/// so add them when the helper is not already found. `exec` keeps exit 127
/// when the helper is missing everywhere.
const REMOTE_STREAM_SCRIPT: &str = "command -v taarof-remote >/dev/null 2>&1 \
     || PATH=\"$HOME/.local/share/mise/shims:$PATH\"; exec taarof-remote stream";

/// What the agent-session probe and the host poll need from the streams.
/// Injected so probe tests never start ssh.
pub(crate) trait RemoteStreams: Send + Sync {
    /// Declare the hosts worth streaming. Starts missing supervisors; hosts
    /// left out stop at their next tick.
    fn ensure(&self, ssh_targets: &[String]);
    /// The latest snapshot from a healthy, fresh stream. `None` means: use
    /// the shell probe.
    fn live(&self, ssh_target: &str) -> Option<LiveHost>;
    /// Why this host uses the shell probe, when the stream gave up on it.
    fn fallback_note(&self, ssh_target: &str) -> Option<String>;
}

/// One fresh stream view. Records are unstamped; the consumer stamps them.
#[derive(Clone, Debug)]
pub(crate) struct LiveHost {
    pub sessions: Vec<AgentSessionRecord>,
    pub stats: Stats,
    /// Set when the helper's latest sample failed. The sessions are then the
    /// last good set.
    pub error: Option<String>,
    pub observed_at: Instant,
    pub observed_at_unix_ms: u64,
    pub metrics: Option<Metrics>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    /// Records stay fresh this long after the last event: two missed default
    /// heartbeats (10 s) plus slack.
    pub stale_after: Duration,
    /// No event for this long ends the connection.
    pub stall_after: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Consecutive failed connections before the shell probe takes over.
    pub failures_before_fallback: u32,
    /// How long a fallback lasts before the stream is tried again.
    pub fallback_retry: Duration,
    /// How long one `ensure` keeps a host wanted.
    pub wanted_for: Duration,
    /// A connection resets the failure count only after delivering a good
    /// snapshot and staying up this long.
    pub healthy_after: Duration,
    pub tick: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(25),
            stall_after: Duration::from_secs(35),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            failures_before_fallback: 3,
            fallback_retry: Duration::from_secs(600),
            wanted_for: Duration::from_secs(300),
            healthy_after: Duration::from_secs(60),
            tick: Duration::from_millis(200),
        }
    }
}

type CommandFactory = dyn Fn(&str) -> Command + Send + Sync;

#[derive(Default)]
struct HostState {
    /// Bumped per connection. A reader from an older connection never writes.
    generation: u64,
    hello: bool,
    /// The last good records and stats. Kept across reconnects, so a failed
    /// first sample on a new connection cannot erase them.
    snapshot: Option<(Vec<AgentSessionRecord>, Stats)>,
    /// When `snapshot` was last confirmed: a good snapshot, or a heartbeat
    /// while the latest sample was good.
    records_at: Option<(Instant, u64)>,
    /// A snapshot, good or failed, arrived on the current connection.
    sampled: bool,
    /// A good snapshot arrived on the current connection.
    good_sample: bool,
    enumeration_failed: bool,
    metrics: Option<Metrics>,
    last_event: Option<Instant>,
    /// Why the shell probe is in use. Kept through the retry, cleared by the
    /// first good snapshot.
    fallback_note: Option<String>,
    /// When the supervisor tries the stream again after a fallback.
    retry_at: Option<Instant>,
    wanted_until: Option<Instant>,
    running: bool,
}

type Slot = Arc<Mutex<HostState>>;

fn lock(slot: &Slot) -> MutexGuard<'_, HostState> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct StreamHub {
    hosts: Mutex<HashMap<String, Slot>>,
    tuning: Tuning,
    command: Arc<CommandFactory>,
}

/// The process-wide hub. Hosts start only when the agent-session probe asks.
pub(crate) fn hub() -> Arc<StreamHub> {
    static HUB: OnceLock<Arc<StreamHub>> = OnceLock::new();
    HUB.get_or_init(|| {
        Arc::new(StreamHub::new(
            Tuning::default(),
            Arc::new(ssh_stream_command),
        ))
    })
    .clone()
}

/// `ssh -T` with no prompts and a bounded connect. Stdin stays open for the
/// stream's lifetime; closing it is how the helper learns to exit.
pub(crate) fn ssh_stream_command(ssh_target: &str) -> Command {
    let mut command = crate::child_process::command("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "--",
        ssh_target,
        &format!(
            "sh -c {}",
            crate::tmux::quote_remote_shell_arg(REMOTE_STREAM_SCRIPT.to_string())
        ),
    ]);
    command
}

impl StreamHub {
    pub(crate) fn new(tuning: Tuning, command: Arc<CommandFactory>) -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
            tuning,
            command,
        }
    }

    fn slot(&self, ssh_target: &str) -> Option<Slot> {
        self.hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(ssh_target)
            .cloned()
    }

    /// Host status from fresh heartbeat metrics, so the host poll can skip its
    /// own ssh round. Load and memory are required, as in the shell probe.
    pub(crate) fn host_status(&self, ssh_target: &str) -> Option<crate::host::HostStatus> {
        crate::host::status_from_metrics(self.live(ssh_target)?.metrics.as_ref()?)
    }
}

impl RemoteStreams for StreamHub {
    fn ensure(&self, ssh_targets: &[String]) {
        let now = Instant::now();
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (target, slot) in hosts.iter() {
            if !ssh_targets.contains(target) {
                lock(slot).wanted_until = Some(now);
            }
        }
        for target in ssh_targets {
            let slot = hosts.entry(target.clone()).or_default().clone();
            let mut state = lock(&slot);
            state.wanted_until = Some(now + self.tuning.wanted_for);
            if state.running {
                continue;
            }
            state.running = true;
            drop(state);
            let (target, worker_slot) = (target.clone(), slot.clone());
            let (tuning, command) = (self.tuning, self.command.clone());
            let spawned = std::thread::Builder::new()
                .name("taarof-remote-stream".into())
                .spawn(move || supervise(&target, &worker_slot, tuning, &*command));
            if spawned.is_err() {
                // The next ensure retries; the shell probe covers this round.
                lock(&slot).running = false;
            }
        }
    }

    fn live(&self, ssh_target: &str) -> Option<LiveHost> {
        let slot = self.slot(ssh_target)?;
        let state = lock(&slot);
        let last_event = state.last_event?;
        if !state.hello || !state.sampled || last_event.elapsed() >= self.tuning.stale_after {
            return None;
        }
        // Never-good records would only overwrite the shell probe's cache.
        let (observed_at, observed_at_unix_ms) = state.records_at?;
        let (sessions, stats) = state.snapshot.clone()?;
        Some(LiveHost {
            sessions,
            stats,
            error: state
                .enumeration_failed
                .then(|| "taarof-remote enumeration failed or timed out".to_string()),
            observed_at,
            observed_at_unix_ms,
            metrics: state.metrics.clone(),
        })
    }

    fn fallback_note(&self, ssh_target: &str) -> Option<String> {
        let slot = self.slot(ssh_target)?;
        let state = lock(&slot);
        state.fallback_note.clone()
    }
}

enum Outcome {
    Unwanted,
    HelperMissing,
    ProtocolMismatch(u32),
    /// `healthy`: the connection delivered a good snapshot and stayed up
    /// for [`Tuning::healthy_after`].
    Ended {
        healthy: bool,
        error: String,
    },
}

fn wanted(slot: &Slot) -> bool {
    lock(slot)
        .wanted_until
        .is_some_and(|until| Instant::now() < until)
}

/// Sleep until `until` in ticks. False when the host stopped being wanted.
fn wait_while_wanted(slot: &Slot, until: Instant, tick: Duration) -> bool {
    while Instant::now() < until {
        if !wanted(slot) {
            return false;
        }
        std::thread::sleep(tick.min(until.saturating_duration_since(Instant::now())));
    }
    wanted(slot)
}

fn supervise(target: &str, slot: &Slot, tuning: Tuning, command: &CommandFactory) {
    let mut failures = 0u32;
    loop {
        let retry_at = lock(slot).retry_at.take();
        if let Some(until) = retry_at {
            if !wait_while_wanted(slot, until, tuning.tick) {
                break;
            }
            // Try the stream again; the note stays until a snapshot arrives.
            failures = 0;
        }
        if !wanted(slot) {
            break;
        }
        let fallback = |note: String| {
            let mut state = lock(slot);
            state.fallback_note = Some(note);
            state.retry_at = Some(Instant::now() + tuning.fallback_retry);
        };
        match connect(target, slot, &tuning, command) {
            Outcome::Unwanted => break,
            Outcome::HelperMissing => {
                fallback(
                    "taarof-remote is not installed on this host; using the shell probe."
                        .to_string(),
                );
            }
            Outcome::ProtocolMismatch(version) => {
                fallback(format!(
                    "taarof-remote speaks protocol {version}, but this app needs \
                     {PROTOCOL_VERSION}; using the shell probe."
                ));
            }
            Outcome::Ended { healthy, error } => {
                if healthy {
                    failures = 0;
                }
                failures += 1;
                if failures >= tuning.failures_before_fallback {
                    fallback(format!(
                        "The taarof-remote stream failed {failures} times in a row \
                         ({error}); using the shell probe."
                    ));
                    continue;
                }
                let backoff = tuning
                    .backoff_base
                    .saturating_mul(1 << (failures - 1).min(16))
                    .min(tuning.backoff_max);
                if !wait_while_wanted(slot, Instant::now() + backoff, tuning.tick) {
                    break;
                }
            }
        }
    }
    let mut state = lock(slot);
    state.running = false;
    state.hello = false;
    state.generation += 1;
}

/// One connection, from spawn to reap. Every path kills and waits the child.
fn connect(target: &str, slot: &Slot, tuning: &Tuning, command: &CommandFactory) -> Outcome {
    let mut command = command(target);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // This function owns the child and reaps it on every path below.
    #[allow(clippy::disallowed_methods)]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome::Ended {
                healthy: false,
                error: format!("could not start ssh: {error}"),
            }
        }
    };
    // Held, never written: EOF on stdin is the helper's signal to exit.
    let stdin = child.stdin.take();
    // Both pipe readers poll this flag, so teardown can end them even when a
    // descendant of ssh still holds a pipe open.
    let cancel = Arc::new(AtomicBool::new(false));
    let stderr = child
        .stderr
        .take()
        .and_then(|pipe| drain_stderr(Cancellable::new(pipe, cancel.clone())));
    let generation = {
        let mut state = lock(slot);
        state.generation += 1;
        state.hello = false;
        state.sampled = false;
        state.good_sample = false;
        state.enumeration_failed = false;
        state.metrics = None;
        state.last_event = None;
        state.generation
    };
    let (ended, reader_end) = mpsc::sync_channel(1);
    let mut reader = None;
    if let Some(stdout) = child.stdout.take() {
        let reader_slot = slot.clone();
        let stdout = Cancellable::new(stdout, cancel.clone());
        match std::thread::Builder::new()
            .name("taarof-remote-stream-read".into())
            .spawn(move || {
                let end = read_events(stdout, &reader_slot, generation);
                let _ = ended.send(end);
            }) {
            Ok(handle) => reader = Some(handle),
            Err(_) => {
                let _ = child.kill();
            }
        }
    }

    let started = Instant::now();
    let mut reader_result = None;
    let mut eof_at: Option<Instant> = None;
    let mut unwanted = false;
    let mut stalled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if reader_result.is_none() {
            match reader_end.try_recv() {
                Ok(end) => {
                    let clean_eof = matches!(end, ReadEnd::Eof);
                    reader_result = Some(end);
                    // On EOF the child is exiting; give it a moment so its
                    // exit code (127 = helper missing) is not lost to a kill.
                    if !clean_eof {
                        break None;
                    }
                    eof_at = Some(Instant::now());
                }
                Err(mpsc::TryRecvError::Disconnected) => break None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        } else if eof_at.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
            break None;
        }
        if !wanted(slot) {
            unwanted = true;
            break None;
        }
        let last = lock(slot).last_event.unwrap_or(started);
        if last.elapsed() > tuning.stall_after {
            stalled = true;
            break None;
        }
        std::thread::sleep(tuning.tick.min(Duration::from_millis(50)));
    };
    let status = match status {
        Some(status) => Some(status),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    };
    drop(stdin);
    // Let the reader finish lines already in the pipe, then cancel both
    // readers and join them. A descendant holding a pipe cannot keep them.
    let reader_result =
        reader_result.or_else(|| reader_end.recv_timeout(Duration::from_millis(500)).ok());
    cancel.store(true, Ordering::Relaxed);
    if let Some(reader) = reader {
        let _ = reader.join();
    }
    let stderr = stderr
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let (saw_hello, healthy) = {
        let mut state = lock(slot);
        let saw_hello = state.hello;
        let healthy = state.good_sample && started.elapsed() >= tuning.healthy_after;
        // Nothing from this connection may read as live any more.
        state.generation += 1;
        state.hello = false;
        state.sampled = false;
        (saw_hello, healthy)
    };
    if unwanted {
        return Outcome::Unwanted;
    }
    match reader_result {
        Some(ReadEnd::Mismatch(version)) => return Outcome::ProtocolMismatch(version),
        Some(ReadEnd::Protocol(message)) => {
            return Outcome::Ended {
                healthy,
                error: message,
            }
        }
        _ => {}
    }
    if status.and_then(|s| s.code()) == Some(127) && !saw_hello {
        return Outcome::HelperMissing;
    }
    Outcome::Ended {
        healthy,
        error: describe_end(stalled, status, &stderr, tuning),
    }
}

fn describe_end(
    stalled: bool,
    status: Option<ExitStatus>,
    stderr: &str,
    tuning: &Tuning,
) -> String {
    if stalled {
        return format!("no event for {}s", tuning.stall_after.as_secs());
    }
    let detail: String = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect();
    let status = match status.and_then(|s| s.code()) {
        Some(code) => format!("exit {code}"),
        None => "ended".to_string(),
    };
    if detail.is_empty() {
        status
    } else {
        format!("{status}: {detail}")
    }
}

/// Keep the first 4 KiB of stderr for the error text; drain the rest so the
/// child never blocks on a full pipe.
fn drain_stderr(mut pipe: impl Read + Send + 'static) -> Option<std::thread::JoinHandle<String>> {
    std::thread::Builder::new()
        .name("taarof-remote-stream-err".into())
        .spawn(move || {
            let mut kept = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = 4096usize.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&kept).into_owned()
        })
        .ok()
}

/// A pipe whose reads give up once `cancel` is set. Data already waiting is
/// still read, for a bounded number of reads, so final lines and error text
/// survive a normal exit.
struct Cancellable<R> {
    inner: R,
    cancel: Arc<AtomicBool>,
    reads_after_cancel: u32,
}

impl<R> Cancellable<R> {
    fn new(inner: R, cancel: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            cancel,
            reads_after_cancel: 0,
        }
    }
}

impl<R: Read + AsRawFd> Read for Cancellable<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let cancelled = self.cancel.load(Ordering::Relaxed);
            if cancelled {
                self.reads_after_cancel += 1;
                if self.reads_after_cancel > 64 {
                    return Ok(0);
                }
            }
            let mut fd = libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = if cancelled { 0 } else { 100 };
            // The descriptor is owned by `inner` and open for this call.
            let ready = unsafe { libc::poll(&mut fd, 1, timeout) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready > 0 {
                return self.inner.read(buf);
            }
            if cancelled {
                return Ok(0);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum ReadEnd {
    Eof,
    Mismatch(u32),
    Protocol(String),
}

fn read_events(stdout: impl Read, slot: &Slot, generation: u64) -> ReadEnd {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_bounded_line(&mut reader, &mut line, MAX_EVENT_LINE_BYTES) {
            Ok(0) => return ReadEnd::Eof,
            Ok(_) => {}
            Err(LineError::TooLong) => {
                return ReadEnd::Protocol("a stream line exceeded 1 MiB".to_string())
            }
            Err(LineError::Io) => return ReadEnd::Eof,
        }
        let Ok(event) = serde_json::from_slice::<Event>(&line) else {
            return ReadEnd::Protocol("a stream line was not a valid event".to_string());
        };
        let mut state = lock(slot);
        if state.generation != generation {
            return ReadEnd::Eof;
        }
        match (state.hello, event) {
            (
                false,
                Event::Hello {
                    protocol_version, ..
                },
            ) => {
                if protocol_version != PROTOCOL_VERSION {
                    return ReadEnd::Mismatch(protocol_version);
                }
                // `hello.host` is display only; identity is the ssh target.
                state.hello = true;
            }
            (false, _) => {
                return ReadEnd::Protocol("the stream did not start with hello".to_string())
            }
            (true, Event::Hello { .. }) => {
                return ReadEnd::Protocol("the stream sent a second hello".to_string())
            }
            (true, Event::Snapshot { sessions, stats }) => {
                state.sampled = true;
                if stats.enumeration_failed {
                    // Keep the last good records and their time; report the
                    // failed sample. This is not a recovery.
                    state.enumeration_failed = true;
                } else {
                    state.enumeration_failed = false;
                    state.good_sample = true;
                    state.snapshot = Some((sessions, stats));
                    state.records_at = Some((Instant::now(), unix_time_ms()));
                    state.fallback_note = None;
                }
            }
            (true, Event::Heartbeat { metrics, .. }) => {
                state.metrics = Some(metrics);
                // Snapshots are sent on change only, so a heartbeat after a
                // good sample confirms the records are still current.
                if state.good_sample && !state.enumeration_failed {
                    state.records_at = Some((Instant::now(), unix_time_ms()));
                }
            }
            (true, Event::Unknown) => {}
        }
        // App clock, not the remote's: it is compared with local times.
        state.last_event = Some(Instant::now());
    }
}

#[derive(Debug, PartialEq)]
enum LineError {
    TooLong,
    Io,
}

/// Read one line, newline included, refusing to buffer more than `max` bytes.
/// Returns 0 at EOF.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max: usize,
) -> Result<usize, LineError> {
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(LineError::Io),
        };
        if available.is_empty() {
            return Ok(line.len());
        }
        let (take, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(LineError::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if done {
            return Ok(line.len());
        }
    }
}

#[cfg(test)]
#[path = "remote_stream_client_tests.rs"]
mod tests;
