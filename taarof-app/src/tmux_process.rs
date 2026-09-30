//! Bounded subprocess seam shared by tmux observation and control callers.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub(crate) const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct Outcome {
    pub(crate) status: Option<ExitStatus>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) error: Option<String>,
    /// Only spawn failures populate this. Callers classify kind/errno rather
    /// than parsing localized error text (for example, retrying ETXTBSY).
    pub(crate) spawn_error: Option<io::Error>,
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
                    serde_json::json!({ "argv": argv, "status": outcome.status.map(|status| status.to_string()), "error": error,
                        "spawn_errno": outcome.spawn_error.as_ref().and_then(io::Error::raw_os_error) }),
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
            spawn_error: None,
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
    let mut command = crate::child_process::command(program);
    command.args(&argv[1..]);
    run_command(command, timeout, limit)
}

/// Run a caller-configured command through the same bounded subprocess seam.
/// Preserves arguments and current_dir, but owns all stdio: null stdin and
/// captured output. Explicit environment additions are sanitized before spawn.
/// This seam returns outcomes only; callers choose safe diagnostic context.
pub(crate) fn run_command(mut command: Command, timeout: Duration, limit: usize) -> Outcome {
    let deadline = Instant::now() + timeout;
    crate::child_env::prepare_child_command(&mut command, &[]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Owned by the try_wait loop, or transferred to the shared reaper on failure.
    #[allow(clippy::disallowed_methods)]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut outcome = Outcome::failed(format!("failed to spawn command: {error}"));
            outcome.spawn_error = Some(error);
            return outcome;
        }
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
        spawn_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            let mut suffix = [0; 8];
            getrandom::getrandom(&mut suffix).unwrap();
            let path = std::env::temp_dir().join(format!(
                "taarof-d08-{}-{:x}",
                std::process::id(),
                u64::from_ne_bytes(suffix)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn shell(script: &str, timeout: Duration, limit: usize) -> Outcome {
        run_with_limit(&["sh".into(), "-c".into(), script.into()], timeout, limit)
    }

    #[test]
    fn configured_command_preserves_cwd_and_safe_env_and_sanitizes_overrides() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("runner-marker"), b"marker").unwrap();
        let mut command = crate::child_process::command("sh");
        command.args(["-c", "test -f runner-marker && test \"$TAAROF_D08_SAFE\" = kept && test -z \"${INFISICAL_TOKEN+x}\" && test -z \"${INFISICAL_SERVICE_TOKEN+x}\" && test -z \"${TAAROF_RELOAD_RESUME_AGENTS+x}\" && printf done"])
            .current_dir(&fixture.0)
            .env("TAAROF_D08_SAFE", "kept")
            .env("INFISICAL_TOKEN", "synthetic-test-token")
            .env("INFISICAL_SERVICE_TOKEN", "synthetic-test-token")
            .env("TAAROF_RELOAD_RESUME_AGENTS", "synthetic-test-marker");
        let outcome = run_command(command, Duration::from_secs(2), 1024);
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.stdout, "done");
        assert!(outcome.spawn_error.is_none());
    }

    #[test]
    fn spawn_failure_exposes_errno_including_retryable_text_file_busy() {
        let fixture = Fixture::new();
        let path = fixture.0.join("fake-command");
        std::fs::write(&path, b"#!/bin/sh\nprintf ready\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let outcome = run_command(
            crate::child_process::command(&path),
            Duration::from_secs(2),
            1024,
        );
        assert_eq!(
            outcome.spawn_error.unwrap().raw_os_error(),
            Some(libc::ETXTBSY)
        );
        assert!(outcome.status.is_none());
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
        drop(writer);
        let outcome = run_command(
            crate::child_process::command(&path),
            Duration::from_secs(2),
            1024,
        );
        assert!(outcome.spawn_error.is_none());
        assert!(outcome.status.unwrap().success());
        assert_eq!(outcome.stdout, "ready");

        let outcome = run_command(
            crate::child_process::command(fixture.0.join("missing")),
            Duration::from_secs(2),
            1024,
        );
        let error = outcome.spawn_error.unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
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
