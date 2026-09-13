//! Real exec boundaries: sibling panes and ordinary helpers must not retain PTYs.
use std::os::unix::fs::DirBuilderExt;
use std::process::Command;
use std::time::{Duration, Instant};
use taarof_app::pty_broker::{PtyBroker, SpawnSpec};

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: None,
        env: Vec::new(),
        cols: 80,
        rows: 24,
    }
}

#[test]
fn sibling_and_helper_exec_do_not_inherit_earlier_pty() {
    let first = PtyBroker::spawn(spec("read -r input; printf 'FIRST:%s\\n' \"$input\""))
        .expect("first child");
    let second = PtyBroker::spawn(spec("read -r input; printf 'SECOND:%s\\n' \"$input\""))
        .expect("second child");
    // These shells have completed exec and block on stdin. All descriptors
    // above stdio must be unrelated to terminal masters or sibling slaves.
    let assert_no_extra_pty = |pid| {
        for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .flatten()
        {
            let fd = entry.file_name().to_string_lossy().parse::<u32>().unwrap();
            if fd > 2 {
                if let Ok(target) = std::fs::read_link(entry.path()) {
                    assert!(
                        !target.starts_with("/dev/pts")
                            && target != std::path::Path::new("/dev/ptmx"),
                        "child inherited terminal descriptor {fd}: {}",
                        target.display()
                    );
                }
            }
        }
    };
    assert_no_extra_pty(second.child_pid());
    // std::process retains arbitrary inherited fds unless the allocator marks
    // them close-on-exec; piped stdout does not hide that boundary.
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            "for fd in /proc/$$/fd/*; do case ${fd##*/} in 0|1|2) continue;; esac; readlink \"$fd\"; done; :",
        ])
        .env_remove("INFISICAL_TOKEN")
        .env_remove("INFISICAL_SERVICE_TOKEN")
        .output()
        .expect("helper exec and reap");
    assert!(output.status.success());
    let inventory = String::from_utf8(output.stdout).unwrap();
    assert!(
        !inventory
            .lines()
            .any(|line| line.starts_with("/dev/pts") || line == "/dev/ptmx"),
        "helper inherited a terminal descriptor: {inventory}"
    );

    for (pane, marker) in [(&first, "FIRST:"), (&second, "SECOND:")] {
        let output = pane.subscribe().expect("native subscription");
        pane.write_input(b"boundary-ok\n")
            .expect("working child stdin");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            assert!(!wait.is_zero(), "child output deadline");
            received.extend(output.recv_timeout(wait).expect("child output"));
            if String::from_utf8_lossy(&received).contains(&format!("{marker}boundary-ok")) {
                break;
            }
        }
    }
    drop(first);
    drop(second);
}

#[test]
fn closed_stdio_and_partial_allocation() {
    const MODE: &str = "TAAROF_TEST_PTY_FD_MODE";
    match std::env::var(MODE).as_deref() {
        Ok("closed-stdio") => {
            // Closing the slave does not mean waitpid can already observe exit.
            // Hold the child alive after EOF until this test explicitly releases
            // it, making that legal ordering deterministic rather than a race.
            let gate_dir =
                std::env::temp_dir().join(format!("taarof-fd-exit-{}", std::process::id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&gate_dir)
                .expect("private exit gate directory");
            let release = gate_dir.join("release");
            let mut child_spec = spec("printf 'STDIO_OK\\n'; exec 0<&- 1>&- 2>&-; while [ ! -e \"$TAAROF_TEST_EXIT_RELEASE\" ]; do sleep 0.01; done");
            child_spec.env.push((
                "TAAROF_TEST_EXIT_RELEASE".into(),
                release.to_string_lossy().into_owned(),
            ));
            // Only this fresh, single-test process changes its descriptor table.
            // Preserve the test reporter's handles outside stdio, close-on-exec.
            let backups: Vec<_> = (0..3)
                .map(|fd| {
                    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
                    assert!(copy >= 3);
                    copy
                })
                .collect();
            for fd in 0..3 {
                unsafe {
                    libc::close(fd);
                }
            }
            let result = PtyBroker::spawn(child_spec);
            for (fd, backup) in backups.into_iter().enumerate() {
                assert_eq!(unsafe { libc::dup2(backup, fd as i32) }, fd as i32);
                unsafe {
                    libc::close(backup);
                }
            }
            let pane = result.expect("spawn with all stdio initially closed");
            let output = pane.subscribe().expect("native subscription");
            let mut bytes = Vec::new();
            loop {
                match output.recv_timeout(Duration::from_secs(5)) {
                    Ok(chunk) => bytes.extend(chunk),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(error) => panic!("child did not close its PTY: {error}"),
                }
            }
            assert!(String::from_utf8_lossy(&bytes).contains("STDIO_OK"));
            let mut status = pane.try_exit_code();
            assert_eq!(status, None, "the child is alive after PTY EOF");
            std::fs::write(&release, []).expect("release child exit");
            let deadline = Instant::now() + Duration::from_secs(5);
            while status.is_none() {
                assert!(Instant::now() < deadline, "child exit status deadline");
                std::thread::sleep(Duration::from_millis(1));
                status = pane.try_exit_code();
            }
            assert_eq!(status, Some(0));
            std::fs::remove_dir_all(gate_dir).expect("remove owned exit gate");
        }
        Ok("partial-allocation") => {
            // Four available numbers allow a master at 0 promoted to 3, then
            // a slave at 0 whose promotion fails. Both must be closed on error.
            assert_eq!(unsafe { libc::fcntl(3, libc::F_GETFD) }, -1);
            let mut old = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) }, 0);
            let limited = libc::rlimit {
                rlim_cur: 4,
                rlim_max: old.rlim_max,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
            unsafe {
                libc::close(0);
            }
            let result = PtyBroker::spawn(spec("exit 0"));
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
            assert!(matches!(result, Err(ref error) if error.raw_os_error() == Some(libc::EMFILE)));
            for fd in [0, 3] {
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_GETFD) },
                    -1,
                    "partial PTY allocation leaked fd {fd}"
                );
            }
        }
        _ => {
            for mode in ["closed-stdio", "partial-allocation"] {
                let result = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "closed_stdio_and_partial_allocation",
                        "--nocapture",
                    ])
                    .env(MODE, mode)
                    .env_remove("INFISICAL_TOKEN")
                    .env_remove("INFISICAL_SERVICE_TOKEN")
                    .output()
                    .expect("isolated fd test process");
                assert!(
                    result.status.success(),
                    "{mode}: stdout={} stderr={}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
    }
}
