use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const OUTPUT_CAP: usize = 256 * 1024;
const STDERR_CAP: usize = 8192;
const LINE_CAP: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(2);
fn nonblocking(fd: i32) -> Result<(), String> {
    // SAFETY: fcntl only changes status flags on a live, owned pipe descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("adapter pipe setup failed".into());
    }
    Ok(())
}
fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, cap: usize) -> Result<bool, String> {
    let mut buffer = [0; 4096];
    // Bound work per poll even for an infinite writer; caller checks deadline.
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + n > cap {
                    return Err("adapter output limit exceeded".into());
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("adapter output read failed".into()),
        }
    }
    Ok(false)
}
pub fn invoke(argv: &[String], request: &[u8]) -> Result<Vec<u8>, String> {
    if request.len() > 16384 {
        return Err("adapter request limit exceeded".into());
    }
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe; the child has not joined a process
    // group, so it establishes a private session without a controlling TTY.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    for key in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|_| "adapter executable unavailable")?;
    let result = (|| {
        let mut stdin = child.stdin.take().ok_or("adapter stdin unavailable")?;
        let mut stdout = child.stdout.take().ok_or("adapter stdout unavailable")?;
        let mut stderr = child.stderr.take().ok_or("adapter stderr unavailable")?;
        nonblocking(stdin.as_raw_fd())?;
        nonblocking(stdout.as_raw_fd())?;
        nonblocking(stderr.as_raw_fd())?;
        let start = Instant::now();
        let mut input = Some(&mut stdin);
        let mut written = 0;
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            if start.elapsed() >= DEADLINE {
                return Err("adapter operation timed out".into());
            }
            if let Some(pipe) = &mut input {
                match pipe.write(&request[written..]) {
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err("adapter request write failed".into()),
                }
                if written == request.len() {
                    input = None;
                }
            }
            // Closing the request stream is part of the protocol. Keep no borrowed pipe alive.
            if input.is_none() {
                break;
            }
            drain(&mut stdout, &mut out, OUTPUT_CAP)?;
            drain(&mut stderr, &mut err, STDERR_CAP)?;
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(stdin);
        loop {
            if start.elapsed() >= DEADLINE {
                return Err("adapter operation timed out".into());
            }
            let out_done = drain(&mut stdout, &mut out, OUTPUT_CAP)?;
            let err_done = drain(&mut stderr, &mut err, STDERR_CAP)?;
            if let Some(status) = child.try_wait().map_err(|_| "adapter wait failed")? {
                if !status.success() {
                    return Err("adapter process failed".into());
                }
                if out_done && err_done {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if out.len() > LINE_CAP || !out.ends_with(b"\n") || out[..out.len() - 1].contains(&b'\n') {
            return Err("adapter requires exactly one bounded JSON line".into());
        }
        Ok(out)
    })();
    // SAFETY: setsid assigned this child its own group. Kill descendants
    // as well, including writers retaining pipes after the direct child exits.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    result
}
