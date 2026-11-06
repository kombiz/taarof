//! Only synthetic session files are read; never the developer's real HOME.
use agent_session_core::remote_enumeration::{
    parse_remote_enumeration_records, remote_enumeration_script,
};
use agent_session_core::remote_stream::{Event, PROTOCOL_VERSION};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Stream {
    child: Child,
    lines: Receiver<String>,
    reader: Option<std::thread::JoinHandle<()>>,
    home: PathBuf,
}
impl Stream {
    fn start() -> Self {
        let home = std::env::temp_dir().join(format!(
            "remote-helper-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (path, content) in [
            (
                ".claude/projects/p/a.jsonl",
                "{\"sessionId\":\"claude-fixture\",\"cwd\":\"/synthetic/project\"}\n",
            ),
            (
                ".codex/sessions/b.jsonl",
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-fixture\",\"cwd\":\"/synthetic/project\"}}\n",
            ),
            (
                ".pi/agent/sessions/c.jsonl",
                "{\"type\":\"session\",\"id\":\"pi-fixture\",\"cwd\":\"/synthetic/project\"}\n",
            ),
            (
                ".kimi-code/session_index.jsonl",
                "{\"sessionId\":\"kimi-fixture\",\"workDir\":\"/synthetic/project\"}\n",
            ),
        ] {
            fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
            fs::write(home.join(path), content).unwrap();
        }
        fs::create_dir(home.join("bin")).unwrap();
        for (program, body) in [("tmux", "printf '1\\n1\\n'"), ("nproc", "printf '4\\n'")] {
            let path = home.join("bin").join(program);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_taarof-remote"))
            .args(["stream", "--heartbeat-seconds", "1", "--poll-seconds", "1"])
            .env_clear()
            .env("HOME", &home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", home.join("bin").display()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pipe = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            reader: Some(reader),
            home,
        }
    }
    fn next(&self) -> Event {
        serde_json::from_str(
            &self
                .lines
                .recv_timeout(Duration::from_secs(10))
                .expect("event deadline"),
        )
        .unwrap()
    }
    fn initial(&self) -> (String, Event) {
        let Event::Hello {
            protocol_version,
            helper_version,
            host,
        } = self.next()
        else {
            panic!("hello first")
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);
        assert_eq!(helper_version, env!("CARGO_PKG_VERSION"));
        assert!(!host.is_empty());
        let snapshot = self.next();
        assert!(matches!(snapshot, Event::Snapshot { .. }));
        (host, snapshot)
    }
    fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "helper exit: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "helper must exit promptly");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        fs::remove_dir_all(&self.home).unwrap();
    }
}

#[test]
fn helper_records_match_shell_probe_and_report_loss() {
    let mut stream = Stream::start();
    let (_host, Event::Snapshot { sessions, stats }) = stream.initial() else {
        unreachable!()
    };
    // Metrics travel on the heartbeat sent right after the first snapshot.
    let Event::Heartbeat { metrics, .. } = stream.next() else {
        panic!("heartbeat with metrics follows the first snapshot")
    };
    // This is the same shared shell/parser entry point used by taarof-app.
    let shell = Command::new("sh")
        .args(["-c", &remote_enumeration_script()])
        .env_clear()
        .env("HOME", &stream.home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(shell.status.success());
    let (expected, counts) =
        parse_remote_enumeration_records(&String::from_utf8(shell.stdout).unwrap());
    assert_eq!(sessions, expected);
    assert_eq!(sessions.len(), 4);
    // Records are unstamped; the consumer stamps them from its SSH target.
    assert!(
        sessions
            .iter()
            .all(|s| s.host.is_none() && s.resume_command.is_none())
    );
    assert_eq!(stats.sessions_omitted, 0);
    assert_eq!(stats.records, counts);
    assert!(!stats.enumeration_failed);
    assert_eq!(metrics.nproc, Some(4));
    assert_eq!(metrics.tmux_session_count, Some(2));
    // A bounded sample drops a whole oversize line and reports its loss.
    fs::write(
        stream.home.join(".codex/sessions/oversize.jsonl"),
        format!("{}\n", "x".repeat(65537)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "loss update deadline");
        if let Event::Snapshot { stats, .. } = stream.next()
            && stats.records.dropped_lines == 1
        {
            assert_eq!(stats.records.blocks_without_session, 1);
            break;
        }
    }
    stream.child.stdin.take();
    stream.exited();
}

#[test]
fn heartbeat_and_stdin_eof() {
    let mut stream = Stream::start();
    stream.initial();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Event::Heartbeat {
            observed_at_unix_ms,
            metrics,
        } = stream.next()
        {
            assert!(observed_at_unix_ms > 0);
            assert_eq!(metrics.nproc, Some(4));
            break;
        }
        assert!(Instant::now() < deadline);
    }
    stream.child.stdin.take();
    stream.exited();
}

#[test]
fn hello_snapshot_then_closed_stdout_exits_cleanly() {
    let mut stream = Stream::start();
    stream.initial();
    // Stop the reader and close its pipe without waiting for a future snapshot.
    // The helper observes POLLERR while idle, before its next heartbeat.
    drop(std::mem::replace(&mut stream.lines, mpsc::channel().1));
    // The reader sends the next event, sees disconnection, and drops stdout.
    stream.reader.take().unwrap().join().unwrap();
    stream.exited();
}

#[test]
fn sigterm_exits_cleanly() {
    let mut stream = Stream::start();
    stream.initial();
    assert_eq!(
        unsafe { libc::kill(stream.child.id() as i32, libc::SIGTERM) },
        0
    );
    stream.exited();
}

#[test]
fn slow_sampler_keeps_heartbeats_and_stdin_shutdown_responsive() {
    let mut stream = Stream::start();
    stream.initial();
    // Replace only this fixture's sampler shell; no real user records or tmux.
    let shell = stream.home.join("bin/sh");
    fs::write(&shell, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    fs::set_permissions(shell, fs::Permissions::from_mode(0o755)).unwrap();
    let start = Instant::now();
    for _ in 0..3 {
        assert!(matches!(stream.next(), Event::Heartbeat { .. }));
        assert!(start.elapsed() < Duration::from_secs(4));
    }
    stream.child.stdin.take();
    stream.exited();
}

#[test]
fn metric_drift_alone_sends_no_new_snapshot() {
    let mut stream = Stream::start();
    stream.initial();
    // Real /proc load and MemAvailable change between the 1 s polls, but the
    // fixture sessions do not: only heartbeats may follow.
    let until = Instant::now() + Duration::from_millis(3500);
    let mut heartbeats = 0;
    while Instant::now() < until {
        match stream.lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match serde_json::from_str::<Event>(&line).unwrap() {
                Event::Heartbeat { .. } => heartbeats += 1,
                other => panic!("unexpected event while sessions are unchanged: {other:?}"),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("stream closed: {error}"),
        }
    }
    assert!(
        heartbeats >= 2,
        "heartbeats keep metrics fresh: {heartbeats}"
    );
    stream.child.stdin.take();
    stream.exited();
}
