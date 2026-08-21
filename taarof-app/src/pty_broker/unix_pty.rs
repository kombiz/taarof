//! Broker-owned Unix pseudoterminal.
//!
//! The broker allocates the PTY explicitly, spawns the child on the slave, and
//! becomes the **sole reader** of the master. Every chunk read from the master
//! is fanned out — inside one ordered critical section — to:
//!
//! 1. every subscriber (the local VTE presentation and any loopback subscribers);
//! 2. the bounded [`ReplayWindow`]; and
//! 3. the headless [`TerminalStateModel`].
//!
//! Feeding all three from the same read in the same locked section is what makes
//! "identical ordered bytes reach local presentation, replay, and state model" a
//! structural guarantee rather than a timing coincidence.
//!
//! This module is intentionally free of GTK/VTE dependencies so it runs under
//! the headless test suite. The glue that turns a subscriber channel into
//! `vte::Terminal::feed` calls, and that routes VTE input/resize back into
//! [`BrokeredPane::write_input`]/[`BrokeredPane::resize`], lives in the
//! terminal/pane modules.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use super::epoch::BrokerEpoch;
use super::replay::ReplayWindow;
use super::screen::TerminalStateModel;

const READ_CHUNK_BYTES: usize = 8 * 1024;

/// `ptsname` returns a pointer into a static buffer, so concurrent callers can
/// clobber one another. Serialize the small allocate-and-copy window.
static PTSNAME_LOCK: Mutex<()> = Mutex::new(());

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
        BrokeredPane::spawn(spec)
    }
}

struct BrokerState {
    replay: ReplayWindow,
    model: TerminalStateModel,
    subscribers: Vec<Sender<Vec<u8>>>,
    closed: bool,
}

/// A live pane whose PTY the broker owns. Cloning is intentionally not
/// supported: exactly one owner drives child lifecycle and the reader thread.
pub struct BrokeredPane {
    epoch: BrokerEpoch,
    master: Arc<OwnedFd>,
    write_lock: Mutex<()>,
    shared: Arc<Mutex<BrokerState>>,
    child: Mutex<Child>,
    child_pid: u32,
    reader: Option<JoinHandle<()>>,
}

impl BrokeredPane {
    fn spawn(spec: SpawnSpec) -> io::Result<Self> {
        if spec.argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SpawnSpec.argv must contain at least the program to execute",
            ));
        }

        let (master, slave) = allocate_pty()?;
        set_winsize(master.as_raw_fd(), spec.cols, spec.rows)?;

        let child = spawn_child(&spec, master.as_raw_fd(), slave.as_raw_fd())?;
        // The parent must drop its slave handle: while any process holds the
        // slave open the master read never reaches EOF, so the reader thread
        // could never observe the child exiting.
        drop(slave);
        let child_pid = child.id();

        let epoch = BrokerEpoch::new()
            .map_err(|err| io::Error::other(format!("broker epoch entropy failed: {err}")))?;
        let shared = Arc::new(Mutex::new(BrokerState {
            replay: ReplayWindow::new(epoch),
            model: TerminalStateModel::new(spec.cols as usize, spec.rows as usize),
            subscribers: Vec::new(),
            closed: false,
        }));
        let master = Arc::new(master);

        let reader = spawn_reader(Arc::clone(&master), Arc::clone(&shared));

        Ok(Self {
            epoch,
            master,
            write_lock: Mutex::new(()),
            shared,
            child: Mutex::new(child),
            child_pid,
            reader: Some(reader),
        })
    }

    /// The connection epoch assigned to this pane's output stream.
    pub fn epoch(&self) -> BrokerEpoch {
        self.epoch
    }

    /// The spawned child's process id.
    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    /// Register a presentation/loopback subscriber. The receiver first replays
    /// every chunk still retained in the bounded window (so a subscriber that
    /// attaches after some output still sees identical ordered bytes, up to the
    /// window bound), then receives live output. If the child has already
    /// exited, the receiver drains the retained bytes and then reports EOF.
    pub fn subscribe(&self) -> Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        let mut state = self.shared.lock().expect("broker state lock poisoned");
        for frame in state.replay.frames() {
            // Send failure only happens if `rx` is already dropped; ignore it.
            let _ = tx.send(frame.payload.clone());
        }
        if !state.closed {
            state.subscribers.push(tx);
        }
        rx
    }

    /// Write input bytes to the PTY master. Returns only after every byte has
    /// been dispatched to the kernel, so callers may treat a successful return
    /// as final PTY dispatch.
    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        let _guard = self.write_lock.lock().expect("broker write lock poisoned");
        let fd = self.master.as_raw_fd();
        let mut offset = 0;
        while offset < bytes.len() {
            let remaining = &bytes[offset..];
            let written = unsafe {
                libc::write(
                    fd,
                    remaining.as_ptr().cast::<libc::c_void>(),
                    remaining.len(),
                )
            };
            if written < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            offset += written as usize;
        }
        Ok(())
    }

    /// Resize the PTY and the headless state model to `cols` x `rows`.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        set_winsize(self.master.as_raw_fd(), cols, rows)?;
        let mut state = self.shared.lock().expect("broker state lock poisoned");
        state.model.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// The number of live presentation/loopback subscribers currently attached.
    /// A dead subscriber is pruned lazily on the next read fan-out, so this
    /// reflects the count as of the most recent output chunk.
    pub fn subscriber_count(&self) -> usize {
        self.shared
            .lock()
            .expect("broker state lock poisoned")
            .subscribers
            .len()
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

    /// Wait for the child to exit and reap it.
    pub fn wait(&self) -> io::Result<ExitStatus> {
        self.child
            .lock()
            .expect("broker child lock poisoned")
            .wait()
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
    /// the reader thread's blocking read returns EOF/EIO and it exits.
    pub fn shutdown(&self) {
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
/// the model, and every live subscriber inside one locked section so all three
/// observe identical bytes in identical order.
fn spawn_reader(master: Arc<OwnedFd>, shared: Arc<Mutex<BrokerState>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let fd = master.as_raw_fd();
        let mut buf = [0u8; READ_CHUNK_BYTES];
        loop {
            let read =
                unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
            if read > 0 {
                let chunk = &buf[..read as usize];
                let mut state = shared.lock().expect("broker state lock poisoned");
                let _ = state.replay.push(SystemTime::now(), chunk);
                state.model.feed(chunk);
                state
                    .subscribers
                    .retain(|subscriber| subscriber.send(chunk.to_vec()).is_ok());
                continue;
            }
            if read == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // EIO on Linux is the master-side signal that the slave closed, i.e.
            // the child exited. Treat any other error as a terminal read fault.
            break;
        }

        let mut state = shared.lock().expect("broker state lock poisoned");
        state.closed = true;
        // Dropping the senders signals EOF to every presentation subscriber.
        state.subscribers.clear();
    })
}

/// Allocate a PTY master/slave pair via the POSIX `posix_openpt` family, which
/// avoids linking `libutil` for `openpty`.
fn allocate_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let master_raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master_raw) };

    if unsafe { libc::grantpt(master_raw) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master_raw) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let slave = {
        let _guard = PTSNAME_LOCK.lock().expect("ptsname lock poisoned");
        let name_ptr = unsafe { libc::ptsname(master_raw) };
        if name_ptr.is_null() {
            return Err(io::Error::last_os_error());
        }
        let name = unsafe { CStr::from_ptr(name_ptr) }.to_owned();
        let slave_raw = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { OwnedFd::from_raw_fd(slave_raw) }
    };

    Ok((master, slave))
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
