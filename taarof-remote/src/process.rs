//! All helper children use this sanitizer and deadline/reaping seam.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in agent_session_core::child_env::AMBIENT_INFISICAL_TOKEN_VARS {
        command.env_remove(name);
    }
    command.env_remove("TAAROF_RELOAD_RESUME_AGENTS");
    command
}

struct ReapedChild(Child);
impl Drop for ReapedChild {
    fn drop(&mut self) {
        // The private process group includes the sampler's shell pipelines.
        // Kill descendants too, then synchronously reap our direct child.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

pub struct Output {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub fn output(
    command: &mut Command,
    timeout: Duration,
    cap: usize,
    mut alive: impl FnMut() -> io::Result<()>,
) -> io::Result<Output> {
    alive()?;
    let mut child = ReapedChild(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?,
    );
    let mut pipe = child.0.stdout.take().expect("piped stdout");
    crate::nonblocking(pipe.as_raw_fd())?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        alive()?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "probe timed out"));
        }
        let mut buffer = [0; 8192];
        let mut received = false;
        match pipe.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                received = true;
                let keep = n.min(cap.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..keep]);
                if n > keep {
                    return Ok(Output {
                        bytes,
                        truncated: true,
                    });
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        if eof && let Some(status) = child.0.try_wait()? {
            return if status.success() {
                Ok(Output {
                    bytes,
                    truncated: false,
                })
            } else {
                Err(io::Error::other("probe failed"))
            };
        }
        if !received {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn children_are_sanitized_bounded_and_reaped() {
        let cmd = command("true");
        for name in [
            "INFISICAL_TOKEN",
            "INFISICAL_SERVICE_TOKEN",
            "TAAROF_RELOAD_RESUME_AGENTS",
        ] {
            assert!(
                cmd.get_envs()
                    .any(|(key, value)| key == name && value.is_none())
            );
        }
        let start = Instant::now();
        let error = output(
            command("sh").args(["-c", "sleep 30 & wait"]),
            Duration::from_millis(60),
            100,
            || Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        let out = output(
            command("sh").args(["-c", "printf 123456789"]),
            Duration::from_secs(1),
            4,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(out.bytes, b"1234");
        assert!(out.truncated);
    }
}
