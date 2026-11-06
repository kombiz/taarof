//! Observe-only remote metadata over SSH stdio. No sockets or control path.
mod process;
mod sample;

use agent_session_core::{
    discovery::unix_time_ms,
    remote_stream::{Event, PROTOCOL_VERSION},
};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

struct NonblockingFd {
    fd: i32,
    flags: i32,
}
impl Drop for NonblockingFd {
    fn drop(&mut self) {
        // Inherited stdout can share an open-file description with the shell.
        // Restore its flags before exit, including on EOF, errors and signals.
        unsafe { libc::fcntl(self.fd, libc::F_SETFL, self.flags) };
    }
}

fn nonblocking(fd: i32) -> io::Result<NonblockingFd> {
    // All callers own a live descriptor; preserve its existing status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NonblockingFd { fd, flags })
}

struct Lifecycle(Arc<AtomicBool>);
impl Lifecycle {
    fn check(&self) -> io::Result<()> {
        if self.0.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: 0,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(error)
            };
        }
        if fds
            .iter()
            .any(|fd| fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            // Input is not a command channel. Drain at most one small chunk.
            let mut bytes = [0u8; 1024];
            let n = unsafe { libc::read(0, bytes.as_mut_ptr().cast(), bytes.len()) };
            if n == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if n < 0 {
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn emit(&self, event: &Event) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            self.check()?;
            let n = unsafe { libc::write(1, remaining.as_ptr().cast(), remaining.len()) };
            if n > 0 {
                remaining = &remaining[n as usize..];
            } else if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            } else {
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}

struct Options {
    poll: Duration,
    heartbeat: Duration,
}
fn options() -> Result<Options, &'static str> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("stream") {
        return Err("usage: taarof-remote stream [--poll-seconds N] [--heartbeat-seconds N]");
    }
    let mut options = Options {
        poll: Duration::from_secs(2),
        heartbeat: Duration::from_secs(10),
    };
    while let Some(flag) = args.next() {
        let seconds = args
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| (1..=3600).contains(v))
            .ok_or("interval must be 1..3600 seconds")?;
        match flag.as_str() {
            "--poll-seconds" => options.poll = Duration::from_secs(seconds),
            "--heartbeat-seconds" => options.heartbeat = Duration::from_secs(seconds),
            _ => return Err("unknown stream option"),
        }
    }
    Ok(options)
}

fn stream(options: Options) -> io::Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopped.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGPIPE, stopped.clone())?;
    let lifecycle = Lifecycle(stopped);
    let _stdout_flags = nonblocking(1)?;
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))?
        .trim()
        .to_owned();
    lifecycle.emit(&Event::Hello {
        protocol_version: PROTOCOL_VERSION,
        helper_version: env!("CARGO_PKG_VERSION").into(),
        host: host.clone(),
    })?;
    let mut previous = None;
    let mut next_poll = Instant::now();
    let mut next_heartbeat = Instant::now() + options.heartbeat;
    loop {
        lifecycle.check()?;
        if Instant::now() >= next_poll {
            let snapshot = sample::snapshot(&host, || lifecycle.check());
            lifecycle.check()?;
            // Compare the bounded, typed state; timestamps are not part of it.
            if previous.as_ref() != Some(&snapshot) {
                lifecycle.emit(&snapshot)?;
                previous = Some(snapshot);
            }
            next_poll = Instant::now() + options.poll;
        }
        if Instant::now() >= next_heartbeat {
            lifecycle.emit(&Event::Heartbeat {
                observed_at_unix_ms: unix_time_ms(),
            })?;
            next_heartbeat = Instant::now() + options.heartbeat;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() -> std::process::ExitCode {
    let options = match options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match stream(options) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("taarof-remote: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
