//! GTK glue that attaches a broker-owned child PTY to a local VTE terminal
//! through a raw-mode PTY conduit.
//!
//! VTE refuses a non-PTY fd as a foreign pty (`foreign_sync` on a socketpair
//! fails with `EINVAL`, verified by `examples/pty_conduit_probe`), so VTE is
//! handed a *second real* PTY master and that pty's slave is placed in raw mode
//! to act as a transparent byte conduit. The broker stays the sole reader of the
//! real child PTY master; two relay threads bridge the conduit:
//!
//! ```text
//! child master --(broker subscribe)--> conduit slave --> VTE master  (render)
//! VTE master  --> conduit slave --(read)--> broker.write_input --> child master
//! ```
//!
//! Because VTE keeps a real pty, it does all of its own input encoding (special
//! keys, application-cursor mode, mouse reporting, bracketed paste) and existing
//! `terminal.feed_child` call sites (broadcast, send-to-pane) keep working — they
//! write into VTE's conduit master, which the input relay forwards to the child.
//!
//! Resize is forwarded from VTE's grid size into [`BrokeredPane::resize`]; child
//! exit is observed by polling the broker from the main loop (see `process`).

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use vte::prelude::*;

use crate::pty_broker::{BrokeredPane, PtyBroker, SpawnSpec};

const CONDUIT_READ_BYTES: usize = 8 * 1024;
const INPUT_POLL_TIMEOUT_MS: libc::c_int = 200;
const OUTPUT_POLL_TIMEOUT_MS: libc::c_int = 50;

/// A live broker attachment for one pane: the broker-owned child PTY plus the
/// two relay threads bridging VTE's conduit. Dropping it tears the pane down.
pub(crate) struct BrokerHandle {
    pane: Arc<BrokeredPane>,
    exited: Arc<AtomicBool>,
    relays: Vec<JoinHandle<()>>,
    last_cols: std::cell::Cell<u16>,
    last_rows: std::cell::Cell<u16>,
    #[cfg(test)]
    teardown_complete: Option<std::sync::mpsc::Sender<()>>,
}

impl BrokerHandle {
    /// The broker-owned child PTY.
    pub(crate) fn pane(&self) -> &Arc<BrokeredPane> {
        &self.pane
    }

    /// The spawned child's process id.
    pub(crate) fn child_pid(&self) -> u32 {
        self.pane.child_pid()
    }

    /// Forward a new grid size to the child PTY and state model. De-duplicated so
    /// a per-frame tick callback stays cheap, and it ignores the zero size VTE
    /// reports before its first allocation.
    pub(crate) fn resize_to(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        if self.last_cols.get() == cols && self.last_rows.get() == rows {
            return;
        }
        self.last_cols.set(cols);
        self.last_rows.set(rows);
        let _ = self.pane.resize(cols, rows);
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        // Pane handles are dropped from GTK callbacks, so teardown must never
        // wait for a child or relay on the caller. Signal cancellation first,
        // then move every potentially blocking operation to a reaper thread.
        // The one documented exception is the reaper-spawn failure path below.
        //
        // On the success path the caller can never become the final `pane`
        // owner: it adds two strong refs (`pane`, `reaper_pane`) and drops
        // exactly two, while the closure holds one the caller never touches.
        // So `BrokeredPane::drop` (a blocking reader join) cannot run here.
        self.exited.store(true, Ordering::SeqCst);
        let pane = Arc::clone(&self.pane);
        let relays = std::mem::take(&mut self.relays);
        let child_pid = pane.child_pid();
        #[cfg(test)]
        let teardown_complete = self.teardown_complete.take();
        let reaper_pane = Arc::clone(&pane);
        let spawned = std::thread::Builder::new()
            .name(reaper_thread_name(child_pid))
            .spawn(move || {
                pane.shutdown();
                for relay in relays {
                    let _ = relay.join();
                }
                // Reap the broker reader on this worker too when this is the
                // final owner; it must never fall back to the GTK caller.
                drop(pane);
                #[cfg(test)]
                if let Some(teardown_complete) = teardown_complete {
                    let _ = teardown_complete.send(());
                }
            });
        if let Err(error) = spawned {
            // The reaper could not be created (e.g. the OS thread limit was
            // hit), so there is nowhere to offload to: teardown has to finish
            // on the caller. Kill the child so it is not leaked. The
            // deadlock-prone relay joins are still skipped -- those handles
            // were moved into the closure, which `spawn` dropped on this
            // thread, detaching the relays to exit on their own within their
            // poll timeouts.
            //
            // Dropping that closure also released its `pane` ref here, so
            // unlike the success path the caller CAN end up the final owner
            // and run `BrokeredPane::drop`, which blocks joining the broker's
            // reader thread. That join is bounded rather than deadlock-prone:
            // `shutdown()` below has already killed the child, so the reader's
            // outstanding `read()` on the real master returns EIO promptly. It
            // is accepted here only because the alternative -- leaking the
            // child, its fds, and the reader thread -- is worse.
            eprintln!("taarof: broker reaper spawn failed for pid {child_pid}: {error}");
            reaper_pane.shutdown();
        }
    }
}

/// Name the teardown thread so `htop`/`ps H`/`/proc/*/comm` identify which
/// child it is reaping.
///
/// Linux caps `comm` at 15 characters plus a NUL and truncates silently, so
/// the whole pid has to fit in what the prefix leaves behind. `pid_max` is
/// itself capped at 2^22 (4194304) on 64-bit Linux, i.e. at most 7 digits,
/// which leaves an 8-character budget for the prefix. `"tb-reap-"` spends
/// exactly that, so even a ceiling pid renders in full.
fn reaper_thread_name(child_pid: u32) -> String {
    format!("tb-reap-{child_pid}")
}

/// Spawn the child under a broker-owned PTY and wire VTE to it through a raw
/// conduit pty. On success the caller stores the returned handle on the pane.
pub(crate) fn attach_broker(terminal: &vte::Terminal, spec: SpawnSpec) -> io::Result<BrokerHandle> {
    let cols = spec.cols;
    let rows = spec.rows;
    let pane = Arc::new(PtyBroker::spawn(spec)?);

    let (conduit_master, conduit_slave) = open_conduit_pty(cols, rows)?;
    let conduit_slave = Arc::new(conduit_slave);

    let pty = vte::Pty::foreign_sync(conduit_master, gio::Cancellable::NONE)
        .map_err(|error| io::Error::other(format!("vte foreign pty attach failed: {error}")))?;
    terminal.set_pty(Some(&pty));

    let exited = Arc::new(AtomicBool::new(false));

    // Output relay: child output -> conduit slave (VTE renders it).
    let output_relay = {
        let rx = pane.subscribe();
        let slave = Arc::clone(&conduit_slave);
        let exited = Arc::clone(&exited);
        std::thread::spawn(move || {
            let fd = slave.as_raw_fd();
            while !exited.load(Ordering::SeqCst) {
                match rx.recv_timeout(std::time::Duration::from_millis(
                    OUTPUT_POLL_TIMEOUT_MS as u64,
                )) {
                    Ok(chunk) => {
                        if write_all_fd_until_cancelled(fd, &chunk, &exited).is_err() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // Any output-relay exit cancels the input side as well. During
            // teardown this may happen before the child has finished reaping.
            exited.store(true, Ordering::SeqCst);
        })
    };

    // Input relay: VTE-encoded input -> broker.write_input -> child.
    let input_relay = {
        let slave = Arc::clone(&conduit_slave);
        let pane = Arc::clone(&pane);
        let exited = Arc::clone(&exited);
        std::thread::spawn(move || {
            let fd = slave.as_raw_fd();
            let mut buf = [0u8; CONDUIT_READ_BYTES];
            loop {
                if exited.load(Ordering::SeqCst) {
                    break;
                }
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut poll_fd, 1, INPUT_POLL_TIMEOUT_MS) };
                if rc < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if rc == 0 {
                    continue;
                }
                if poll_fd.revents & libc::POLLIN != 0 {
                    let read = unsafe {
                        libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len())
                    };
                    if read < 0 {
                        let error = io::Error::last_os_error();
                        if matches!(
                            error.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) {
                            continue;
                        }
                        break;
                    }
                    if read == 0 {
                        break;
                    }
                    if pane.write_input(&buf[..read as usize]).is_err() {
                        break;
                    }
                } else if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    break;
                }
            }
        })
    };

    Ok(BrokerHandle {
        pane,
        exited,
        relays: vec![output_relay, input_relay],
        last_cols: std::cell::Cell::new(cols),
        last_rows: std::cell::Cell::new(rows),
        #[cfg(test)]
        teardown_complete: None,
    })
}

/// Allocate the conduit PTY and put its slave in raw mode so it is a transparent
/// byte pipe (no echo, no CR/LF cooking) between the broker and VTE's master.
fn open_conduit_pty(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let master_raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
    if unsafe { libc::grantpt(master_raw) } < 0 || unsafe { libc::unlockpt(master_raw) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let name_ptr = unsafe { libc::ptsname(master_raw) };
    if name_ptr.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { CStr::from_ptr(name_ptr) }.to_owned();
    let slave_raw = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };

    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(slave_raw, &mut termios) } == 0 {
        unsafe { libc::cfmakeraw(&mut termios) };
        unsafe { libc::tcsetattr(slave_raw, libc::TCSANOW, &termios) };
    }
    set_fd_nonblocking(slave_raw)?;

    set_conduit_winsize(master_raw, cols, rows);
    Ok((master, slave))
}

fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_conduit_winsize(fd: RawFd, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
}

fn write_all_fd_until_cancelled(fd: RawFd, bytes: &[u8], exited: &AtomicBool) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        if exited.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "conduit write cancelled",
            ));
        }
        let remaining = &bytes[offset..];
        let written = unsafe {
            libc::write(
                fd,
                remaining.as_ptr().cast::<libc::c_void>(),
                remaining.len(),
            )
        };
        if written > 0 {
            offset += written as usize;
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "conduit write returned zero",
            ));
        }

        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == io::ErrorKind::WouldBlock {
                // Wait for the conduit to drain, only retrying the write once
                // POLLOUT is actually signalled. A bare timeout or a spurious
                // wake (rc > 0 with revents == 0) would otherwise burn a
                // guaranteed-EAGAIN write syscall. Cancellation is re-checked
                // on every bounded poll cycle so teardown stays prompt.
                loop {
                    if exited.load(Ordering::SeqCst) {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "conduit write cancelled",
                        ));
                    }
                    let mut poll_fd = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut poll_fd, 1, OUTPUT_POLL_TIMEOUT_MS) };
                    if rc < 0 {
                        let poll_error = io::Error::last_os_error();
                        if poll_error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(poll_error);
                    }
                    if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "conduit closed while waiting to write",
                        ));
                    }
                    if poll_fd.revents & libc::POLLOUT != 0 {
                        break;
                    }
                    // Timeout or spurious wake without writability: re-poll
                    // rather than retrying the write into a guaranteed EAGAIN.
                }
                continue;
            }
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_all_fd(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let written = unsafe {
            libc::write(
                fd,
                bytes[offset..].as_ptr().cast::<libc::c_void>(),
                bytes.len() - offset,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        offset += written as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        open_conduit_pty, reaper_thread_name, write_all_fd, write_all_fd_until_cancelled,
        BrokerHandle,
    };
    use crate::pty_broker::{PtyBroker, SpawnSpec};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    static CONDUIT_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fill_conduit_without_reading_master(fd: libc::c_int) {
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(original_flags >= 0, "F_GETFL should succeed");
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) },
            0,
            "F_SETFL O_NONBLOCK should succeed"
        );

        let bytes = [b'x'; 8 * 1024];
        loop {
            let written =
                unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
            if written > 0 {
                continue;
            }
            let error = std::io::Error::last_os_error();
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::WouldBlock,
                "a full unread conduit should report WouldBlock, got {error:?}"
            );
            break;
        }

        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags & !libc::O_NONBLOCK) },
            0,
            "restoring blocking mode should succeed"
        );
    }

    #[test]
    fn dropping_broker_handle_does_not_wait_for_a_blocked_relay() {
        let _guard = CONDUIT_TEST_LOCK
            .lock()
            .expect("test lock should be healthy");
        let (master, slave) = open_conduit_pty(80, 24).expect("conduit pty should allocate");
        fill_conduit_without_reading_master(slave.as_raw_fd());

        let (relay_started_tx, relay_started_rx) = mpsc::channel();
        let relay = std::thread::spawn(move || {
            relay_started_tx.send(()).expect("test should be listening");
            let _ = write_all_fd(slave.as_raw_fd(), b"blocked");
        });
        relay_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("relay should start");

        let pane = Arc::new(
            PtyBroker::spawn(SpawnSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
                cwd: None,
                env: Vec::new(),
                cols: 80,
                rows: 24,
            })
            .expect("test child should spawn"),
        );
        let handle = BrokerHandle {
            pane,
            exited: Arc::new(AtomicBool::new(false)),
            relays: vec![relay],
            last_cols: std::cell::Cell::new(80),
            last_rows: std::cell::Cell::new(24),
            teardown_complete: None,
        };

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let drop_thread = std::thread::spawn(move || {
            drop(handle);
            dropped_tx.send(()).expect("test should be listening");
        });
        let drop_result = dropped_rx.recv_timeout(Duration::from_millis(200));

        // Release the intentionally blocked legacy relay so a RED test still
        // cleans up its thread and child before reporting the failure.
        drop(master);
        drop_thread.join().expect("drop thread should not panic");

        assert!(
            drop_result.is_ok(),
            "dropping a broker handle must not join a blocked relay on the caller"
        );
    }

    #[test]
    fn cancelling_a_full_conduit_write_reclaims_the_relay() {
        let _guard = CONDUIT_TEST_LOCK
            .lock()
            .expect("test lock should be healthy");
        let (master, slave) = open_conduit_pty(80, 24).expect("conduit pty should allocate");
        fill_conduit_without_reading_master(slave.as_raw_fd());
        let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL should succeed");
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0,
            "F_SETFL O_NONBLOCK should succeed"
        );

        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_relay = Arc::clone(&exited);
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        // A payload larger than any PTY buffer so the write can never drain on
        // its own; only cancellation can release the relay. A tiny payload is
        // unreliable: a PTY accepts trickle writes even when "full" (its flip
        // buffer frees space asynchronously), so a small write often completes
        // before cancellation is observed and the test flakes.
        let payload = vec![b'x'; 1024 * 1024];
        let relay = std::thread::spawn(move || {
            started_tx.send(()).expect("test should be listening");
            let result =
                write_all_fd_until_cancelled(slave.as_raw_fd(), &payload, &exited_for_relay);
            finished_tx.send(result).expect("test should be listening");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("relay should start");

        exited.store(true, Ordering::SeqCst);
        let result = finished_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a cancelled full-conduit write should terminate");

        drop(master);
        relay.join().expect("relay should not panic");
        assert_eq!(
            result
                .expect_err("cancellation should stop the write")
                .kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn dropping_broker_handle_reaps_a_full_conduit_relay() {
        let _guard = CONDUIT_TEST_LOCK
            .lock()
            .expect("test lock should be healthy");
        let (master, slave) = open_conduit_pty(80, 24).expect("conduit pty should allocate");
        fill_conduit_without_reading_master(slave.as_raw_fd());
        let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0, "F_GETFL should succeed");
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0,
            "F_SETFL O_NONBLOCK should succeed"
        );

        let slave = Arc::new(slave);
        let slave_reclaimed = Arc::downgrade(&slave);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_relay = Arc::clone(&exited);
        let (started_tx, started_rx) = mpsc::channel();
        let relay = std::thread::spawn(move || {
            started_tx.send(()).expect("test should be listening");
            let _ = write_all_fd_until_cancelled(slave.as_raw_fd(), b"blocked", &exited_for_relay);
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("relay should start");

        let pane = Arc::new(
            PtyBroker::spawn(SpawnSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
                cwd: None,
                env: Vec::new(),
                cols: 80,
                rows: 24,
            })
            .expect("test child should spawn"),
        );
        let (teardown_tx, teardown_rx) = mpsc::channel();
        let handle = BrokerHandle {
            pane,
            exited,
            relays: vec![relay],
            last_cols: std::cell::Cell::new(80),
            last_rows: std::cell::Cell::new(24),
            teardown_complete: Some(teardown_tx),
        };

        drop(handle);
        teardown_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("background teardown should reclaim the relay");
        assert!(
            slave_reclaimed.upgrade().is_none(),
            "joining the relay should release its conduit descriptor"
        );

        // Keep VTE's stand-in master alive until teardown has completed. This
        // proves cancellation, rather than master closure, released the relay.
        drop(master);
    }

    // Linux silently truncates `comm` to 15 chars, which would corrupt exactly
    // the pid the name exists to expose. Pin the budget so a longer prefix
    // cannot creep back in.
    #[test]
    fn reaper_thread_name_keeps_the_whole_pid_for_any_linux_pid() {
        const COMM_LIMIT: usize = 15;
        // 2^22, the largest `kernel.pid_max` 64-bit Linux accepts.
        const PID_CEILING: u32 = 4_194_304;

        for pid in [1, 999, 2_735_213, PID_CEILING] {
            let name = reaper_thread_name(pid);
            assert!(
                name.len() <= COMM_LIMIT,
                "reaper name {name:?} is {} chars, over the {COMM_LIMIT}-char comm limit",
                name.len()
            );
            assert!(
                name.ends_with(&pid.to_string()),
                "reaper name {name:?} must carry the full pid {pid}"
            );
        }
    }

    // Allocating a PTY is pure libc (no GTK/VTE), so this runs headlessly.
    #[test]
    fn conduit_slave_is_raw_no_echo_no_flow_control() {
        let _guard = CONDUIT_TEST_LOCK
            .lock()
            .expect("test lock should be healthy");
        let (_master, slave) = open_conduit_pty(80, 24).expect("conduit pty should allocate");
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut termios) },
            0,
            "tcgetattr should succeed on the conduit slave"
        );
        // Local echo off: the child PTY echoes typed input, so a conduit that
        // also echoed would double every character the operator types.
        assert_eq!(
            termios.c_lflag & libc::ECHO,
            0,
            "conduit slave must not echo (double-echo guard)"
        );
        // XON/XOFF off: the conduit must pass ^S/^Q through as bytes, not treat
        // them as flow control.
        assert_eq!(
            termios.c_iflag & libc::IXON,
            0,
            "conduit slave must not do XON/XOFF flow control"
        );
        // Non-canonical: bytes flow through untouched, with no line buffering.
        assert_eq!(
            termios.c_lflag & libc::ICANON,
            0,
            "conduit slave must be in raw (non-canonical) mode"
        );
    }
}
