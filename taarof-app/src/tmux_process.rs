//! Bounded subprocess seam shared by tmux observation and control callers.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub(crate) const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct Outcome {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) error: Option<String>,
}

/// Observation callers keep their component and failure policy outside the
/// subprocess seam. Transport failures always take precedence over exit status.
pub(crate) fn result(
    outcome: Outcome,
    argv: &[String],
    component: Option<&str>,
) -> Result<String, String> {
    let error = outcome.error.or_else(|| {
        outcome
            .status
            .filter(|status| !status.success())
            .map(|status| {
                if outcome.stderr.trim().is_empty() {
                    format!("command exited with status {status}")
                } else {
                    format!(
                        "command exited with status {status}: {}",
                        outcome.stderr.trim()
                    )
                }
            })
    });
    if let Some(error) = error {
        if let Some(component) = component {
            crate::diagnostics::record_command_failure(
                component,
                "tmux-command",
                &error,
                Some(
                    serde_json::json!({ "argv": argv, "status": outcome.status.map(|status| status.to_string()), "error": error }),
                ),
            );
        }
        Err(error)
    } else {
        Ok(outcome.stdout)
    }
}

impl Outcome {
    fn failed(error: String) -> Self {
        Self {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(error),
        }
    }
}

struct Pipe<R> {
    reader: R,
    bytes: Vec<u8>,
    eof: bool,
    overflow: bool,
}

impl<R: Read + AsRawFd> Pipe<R> {
    fn new(reader: R) -> io::Result<Self> {
        // SAFETY: the owned pipe descriptor remains valid for both fcntl calls.
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            reader,
            bytes: Vec::new(),
            eof: false,
            overflow: false,
        })
    }

    fn drain(&mut self, limit: usize, deadline: Instant) -> io::Result<()> {
        let mut buffer = [0; 8192];
        // Fairness: even a continuously writing child cannot starve the other
        // pipe, wait polling, or the deadline. Excess bytes are discarded.
        for _ in 0..32 {
            if self.eof || Instant::now() >= deadline {
                break;
            }
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => {
                    let keep = count.min(limit.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.overflow |= keep < count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn cleanup(mut child: Child) -> Option<String> {
    let error = child
        .kill()
        .err()
        .map(|error| format!("failed to kill command: {error}"));
    // Waiting is delegated so cleanup cannot extend the caller's deadline.
    crate::child_process::reap(child);
    error
}

pub(crate) fn run(argv: &[String], timeout: Duration) -> Outcome {
    run_with_limit(argv, timeout, OUTPUT_LIMIT)
}

fn run_with_limit(argv: &[String], timeout: Duration, limit: usize) -> Outcome {
    let Some(program) = argv.first() else {
        return Outcome::failed("command argv was empty".into());
    };
    let deadline = Instant::now() + timeout;
    let mut command = crate::child_process::command(program);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Owned by the try_wait loop, or transferred to the shared reaper on failure.
    #[allow(clippy::disallowed_methods)]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Outcome::failed(format!("failed to spawn command: {error}")),
    };
    let pipes = (|| {
        let stdout = Pipe::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing stdout pipe"))?,
        )?;
        let stderr = Pipe::new(
            child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("missing stderr pipe"))?,
        )?;
        Ok::<_, io::Error>((stdout, stderr))
    })();
    let (mut stdout, mut stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            let cleanup_error = cleanup(child)
                .map(|error| format!("; {error}"))
                .unwrap_or_default();
            return Outcome::failed(format!(
                "failed to capture command output: {error}{cleanup_error}"
            ));
        }
    };
    let mut status = None;
    let mut error = loop {
        if Instant::now() >= deadline {
            break Some(format!(
                "command timed out after {:.3}s",
                timeout.as_secs_f64()
            ));
        }
        if let Err(error) = stdout
            .drain(limit, deadline)
            .and_then(|()| stderr.drain(limit, deadline))
        {
            break Some(format!("failed to read command output: {error}"));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => break Some(format!("failed to wait for command: {error}")),
            }
        }
        if status.is_some() && stdout.eof && stderr.eof {
            break (stdout.overflow || stderr.overflow)
                .then(|| format!("command output exceeded {limit} bytes per stream"));
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    if status.is_none() {
        if let Some(cleanup_error) = cleanup(child) {
            error = Some(format!("{}; {cleanup_error}", error.unwrap_or_default()));
        }
    }
    Outcome {
        status,
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str, timeout: Duration, limit: usize) -> Outcome {
        run_with_limit(&["sh".into(), "-c".into(), script.into()], timeout, limit)
    }

    #[test]
    fn success_nonzero_and_spawn_failure() {
        let outcome = shell(
            "printf output; printf detail >&2; exit 7",
            Duration::from_secs(2),
            1024,
        );
        assert_eq!(outcome.status.unwrap().code(), Some(7));
        assert_eq!(outcome.stdout, "output");
        assert_eq!(outcome.stderr, "detail");
        assert!(outcome.error.is_none());
        let outcome = shell(
            "read value && exit 1; printf success",
            Duration::from_secs(2),
            1024,
        );
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.stdout, "success"); // stdin is explicitly null.
        assert!(run(&[], Duration::from_secs(1))
            .error
            .unwrap()
            .contains("empty"));
        assert!(run(
            &["/nonexistent/taarof-d08-command".into()],
            Duration::from_secs(1)
        )
        .error
        .unwrap()
        .contains("spawn"));
    }

    #[test]
    fn both_streams_are_capped_and_excess_is_drained() {
        let outcome = shell(
            "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2",
            Duration::from_secs(2),
            65536,
        );
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.stdout.len(), 65536);
        assert_eq!(outcome.stderr.len(), 65536);
        assert!(outcome.error.unwrap().contains("exceeded"));
    }

    #[test]
    fn deadline_covers_descendant_held_pipes_after_exit_and_kill() {
        for script in ["sleep 1 & exit 0", "sleep 1 & wait"] {
            let start = Instant::now();
            let outcome = shell(script, Duration::from_millis(80), 1024);
            assert!(outcome.error.unwrap().contains("timed out"));
            assert!(start.elapsed() < Duration::from_millis(600));
        }
    }

    #[test]
    fn caller_timeouts_and_continuous_output_are_bounded() {
        let outcome = shell("sleep 0.15; printf done", Duration::from_millis(40), 1024);
        assert!(outcome.error.unwrap().contains("timed out"));
        let outcome = shell("sleep 0.15; printf done", Duration::from_secs(2), 1024);
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.stdout, "done");
        let start = Instant::now();
        let outcome = shell("exec yes x", Duration::from_millis(80), 1024);
        assert_eq!(outcome.stdout.len(), 1024);
        assert!(outcome.error.unwrap().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(600));
    }

    #[test]
    fn command_children_have_no_ambient_token_or_reload_marker() {
        // Give an isolated test process synthetic ambient credentials; never
        // mutate the environment shared by concurrent unit tests.
        let status = crate::child_process::command(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tmux_process::tests::sanitizer_helper",
                "--test-threads=1",
            ])
            .env("TAAROF_D08_SANITIZER_HELPER", "1")
            .env("INFISICAL_TOKEN", "synthetic-test-token")
            .env("INFISICAL_SERVICE_TOKEN", "synthetic-test-token")
            .env("TAAROF_RELOAD_RESUME_AGENTS", "synthetic-test-marker")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn sanitizer_helper() {
        if std::env::var_os("TAAROF_D08_SANITIZER_HELPER").is_none() {
            return;
        }
        assert!(std::env::var_os("INFISICAL_TOKEN").is_some());
        assert!(std::env::var_os("INFISICAL_SERVICE_TOKEN").is_some());
        assert!(std::env::var_os("TAAROF_RELOAD_RESUME_AGENTS").is_some());
        let outcome = shell(
            "test -z \"${INFISICAL_TOKEN+x}\" && test -z \"${INFISICAL_SERVICE_TOKEN+x}\" && test -z \"${TAAROF_RELOAD_RESUME_AGENTS+x}\"",
            Duration::from_secs(2), 1024);
        assert!(outcome.status.unwrap().success());
    }
}
