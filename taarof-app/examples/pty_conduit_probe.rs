//! De-risking probe for the PTY-broker VTE-conduit shim (Task 6 cutover).
//!
//! The live cutover must keep VTE doing its own input encoding and local
//! rendering while the broker is the sole reader of the *real* child PTY master.
//! To do that VTE is handed a separate fd as a foreign PTY and the broker relays
//! bytes between that fd's peer and the real child PTY. Which fd works?
//!
//! This probe tries two candidate conduits and prints machine-greppable markers:
//!
//!   1. `socketpair` — cheapest, but VTE may reject a non-PTY fd.
//!   2. a second real PTY pair with the slave in raw mode — heavier, but VTE
//!      accepts real PTY masters.
//!
//! Run inside the kasm visual container (needs an X display for `gtk::init`):
//!
//!   DISPLAY=:1 cargo +stable run --example pty_conduit_probe \
//!       --manifest-path taarof-app/Cargo.toml
//!
//! Markers per candidate: `..._FOREIGN_SYNC_OK` / `..._FOREIGN_SYNC_ERR=...`,
//! then `..._RESULT=RENDER_OK|RENDER_MISSING` from what VTE parsed.

use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use vte::prelude::*;

fn main() {
    let mut initialized = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if gtk::init().is_ok() {
            initialized = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !initialized {
        println!("PROBE_GTK_INIT_ERR=no display available for gtk::init");
        return;
    }

    probe_socketpair();
    probe_real_pty_conduit();
}

/// Pump the GTK main loop briefly so VTE drains and parses its foreign fd, then
/// return VTE's first grid row as text.
fn pump_and_read(terminal: &vte::Terminal) -> String {
    let ctx = glib::MainContext::default();
    let pump_deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < pump_deadline {
        while ctx.pending() {
            ctx.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal
        .text_range_format(vte::Format::Text, 0, 0, 0, 40)
        .0
        .map(|g| g.to_string())
        .unwrap_or_default()
}

fn write_fd(fd: i32, bytes: &[u8]) -> isize {
    unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) }
}

fn probe_socketpair() {
    let mut fds = [0_i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        println!("SOCKETPAIR_ERR={}", std::io::Error::last_os_error());
        return;
    }
    let vte_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let glue_fd = fds[1];

    match vte::Pty::foreign_sync(vte_fd, gio::Cancellable::NONE) {
        Ok(pty) => {
            println!("SOCKETPAIR_FOREIGN_SYNC_OK");
            let terminal = vte::Terminal::new();
            terminal.set_pty(Some(&pty));
            let _ = write_fd(glue_fd, b"hello-from-socketpair\r\n");
            let text = pump_and_read(&terminal);
            println!("SOCKETPAIR_VTE_TEXT={text:?}");
            println!(
                "SOCKETPAIR_RESULT={}",
                if text.contains("hello-from-socketpair") {
                    "RENDER_OK"
                } else {
                    "RENDER_MISSING"
                }
            );
        }
        Err(error) => println!("SOCKETPAIR_FOREIGN_SYNC_ERR={error}"),
    }
    unsafe { libc::close(glue_fd) };
}

fn probe_real_pty_conduit() {
    // A real PTY pair: VTE gets the master, we feed/read the raw slave.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        println!("REALPTY_OPENPT_ERR={}", std::io::Error::last_os_error());
        return;
    }
    if unsafe { libc::grantpt(master) } < 0 || unsafe { libc::unlockpt(master) } < 0 {
        println!("REALPTY_GRANT_ERR={}", std::io::Error::last_os_error());
        return;
    }
    let name_ptr = unsafe { libc::ptsname(master) };
    if name_ptr.is_null() {
        println!("REALPTY_PTSNAME_ERR={}", std::io::Error::last_os_error());
        return;
    }
    let name = unsafe { CStr::from_ptr(name_ptr) }.to_owned();
    let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        println!("REALPTY_OPEN_SLAVE_ERR={}", std::io::Error::last_os_error());
        return;
    }

    // Put the conduit slave in raw mode so it is a transparent byte pipe (no
    // echo, no CR/LF cooking) between the broker and VTE's master.
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(slave, &mut termios) } == 0 {
        unsafe { libc::cfmakeraw(&mut termios) };
        unsafe { libc::tcsetattr(slave, libc::TCSANOW, &termios) };
    }

    let master_owned = unsafe { OwnedFd::from_raw_fd(master) };
    match vte::Pty::foreign_sync(master_owned, gio::Cancellable::NONE) {
        Ok(pty) => {
            println!("REALPTY_FOREIGN_SYNC_OK");
            let terminal = vte::Terminal::new();
            terminal.set_pty(Some(&pty));
            let _ = write_fd(slave, b"hello-from-real-pty\r\n");
            let text = pump_and_read(&terminal);
            println!("REALPTY_VTE_TEXT={text:?}");
            println!(
                "REALPTY_RESULT={}",
                if text.contains("hello-from-real-pty") {
                    "RENDER_OK"
                } else {
                    "RENDER_MISSING"
                }
            );
        }
        Err(error) => println!("REALPTY_FOREIGN_SYNC_ERR={error}"),
    }
    unsafe { libc::close(slave) };
}
