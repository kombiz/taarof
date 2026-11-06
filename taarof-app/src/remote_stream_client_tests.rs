//! Stream supervisor tests. Each host's "ssh" is a local `sh` fixture, so no
//! test touches the network. Fixtures end in `exec sleep` so a kill reaches
//! the process that holds the pipe.
use super::*;
use agent_session_core::remote_stream::{event_line, Memory};
use std::sync::atomic::{AtomicBool, Ordering};

fn tuning() -> Tuning {
    Tuning {
        stale_after: Duration::from_millis(800),
        stall_after: Duration::from_millis(1200),
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(80),
        failures_before_fallback: 3,
        fallback_retry: Duration::from_secs(60),
        wanted_for: Duration::from_secs(60),
        healthy_after: Duration::from_secs(5),
        tick: Duration::from_millis(10),
    }
}

fn line(event: &Event) -> String {
    let text = String::from_utf8(event_line(event).unwrap()).unwrap();
    assert!(!text.contains('\''), "fixture JSON must not need quoting");
    text.trim_end().to_string()
}

fn hello(version: u32) -> String {
    line(&Event::Hello {
        protocol_version: version,
        helper_version: "0.1.0".into(),
        host: "kernel-name".into(),
    })
}

fn record(id: &str) -> AgentSessionRecord {
    AgentSessionRecord {
        agent: "codex".into(),
        session_id: id.into(),
        title: "fixture".into(),
        cwd: "/srv/project".into(),
        host: None,
        repo_root: None,
        started_at_unix_ms: None,
        updated_at_unix_ms: 1,
        last_user_message_at_unix_ms: None,
        status: "recent".into(),
        live_binding: None,
        resume_command: None,
        resume_unavailable_reason: None,
    }
}

fn snapshot(ids: &[&str], failed: bool) -> String {
    line(&Event::Snapshot {
        sessions: ids.iter().map(|id| record(id)).collect(),
        stats: Stats {
            enumeration_failed: failed,
            ..Stats::default()
        },
    })
}

fn heartbeat() -> String {
    line(&Event::Heartbeat {
        observed_at_unix_ms: 1,
        metrics: Metrics {
            load: Some(1.0),
            mem: Some(Memory {
                total_bytes: 100,
                used_bytes: 25,
            }),
            nproc: Some(4),
            tmux_session_count: Some(3),
        },
    })
}

/// Print `lines`, then run `tail` (for example `exec sleep 30`).
fn script(lines: &[String], tail: &str) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str(&format!("printf '%s\\n' '{line}'; "));
    }
    text.push_str(tail);
    text
}

/// A hub whose "ssh" for each host runs `scripts(host, attempt)` in `sh`.
fn hub_with(
    scripts: impl Fn(&str, usize) -> String + Send + Sync + 'static,
) -> (Arc<StreamHub>, Arc<Mutex<HashMap<String, usize>>>) {
    let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
    let counter = attempts.clone();
    let hub = StreamHub::new(
        tuning(),
        Arc::new(move |host: &str| {
            let attempt = {
                let mut attempts = counter.lock().unwrap();
                let entry = attempts.entry(host.to_string()).or_default();
                *entry += 1;
                *entry
            };
            let mut command = crate::child_process::command("sh");
            command.args(["-c", &scripts(host, attempt)]);
            command
        }),
    );
    (Arc::new(hub), attempts)
}

fn attempts(map: &Mutex<HashMap<String, usize>>, host: &str) -> usize {
    map.lock().unwrap().get(host).copied().unwrap_or(0)
}

fn wait_until(label: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {label}");
}

fn stop(hub: &StreamHub, hosts: &[&str]) {
    hub.ensure(&[]);
    for host in hosts {
        let slot = hub.slot(host).expect("slot");
        wait_until("supervisor exit", || !lock(&slot).running);
    }
}

#[test]
fn live_stream_serves_unstamped_snapshot_and_heartbeat_metrics() {
    let (hub, _) = hub_with(|_, _| {
        script(
            &[
                hello(PROTOCOL_VERSION),
                r#"{"type":"from_the_future","x":1}"#.to_string(),
                snapshot(&["a", "b"], false),
                heartbeat(),
            ],
            "exec sleep 30",
        )
    });
    hub.ensure(&["box.ts".to_string()]);
    wait_until("live with metrics", || {
        hub.live("box.ts")
            .is_some_and(|live| live.metrics.is_some())
    });
    let live = hub.live("box.ts").unwrap();
    assert_eq!(live.sessions.len(), 2);
    assert!(live.sessions.iter().all(|s| s.host.is_none()));
    assert!(live.error.is_none());
    assert_eq!(hub.fallback_note("box.ts"), None);
    let status = hub.host_status("box.ts").expect("metrics status");
    assert_eq!(status.session_count, 3);
    assert!((status.cpu_load_percent - 25.0).abs() < 0.01);
    assert!((status.memory_used_percent - 25.0).abs() < 0.01);
    stop(&hub, &["box.ts"]);
    assert!(hub.live("box.ts").is_none(), "a stopped stream is not live");
}

#[test]
fn missing_helper_falls_back_with_a_note_and_no_retry_storm() {
    let (hub, tries) = hub_with(|_, _| "echo 'taarof-remote: not found' >&2; exit 127".into());
    hub.ensure(&["bare.ts".to_string()]);
    wait_until("fallback note", || hub.fallback_note("bare.ts").is_some());
    assert!(hub
        .fallback_note("bare.ts")
        .unwrap()
        .contains("not installed"));
    assert!(hub.live("bare.ts").is_none());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        attempts(&tries, "bare.ts"),
        1,
        "fallback waits for its retry"
    );
    stop(&hub, &["bare.ts"]);
}

#[test]
fn protocol_mismatch_falls_back_and_kills_the_connection() {
    let (hub, tries) = hub_with(|_, _| script(&[hello(PROTOCOL_VERSION + 1)], "exec sleep 30"));
    hub.ensure(&["new.ts".to_string()]);
    wait_until("fallback note", || hub.fallback_note("new.ts").is_some());
    let note = hub.fallback_note("new.ts").unwrap();
    assert!(
        note.contains(&format!("protocol {}", PROTOCOL_VERSION + 1)),
        "{note}"
    );
    assert_eq!(attempts(&tries, "new.ts"), 1);
    stop(&hub, &["new.ts"]);
}

#[test]
fn repeated_bad_streams_back_off_then_fall_back() {
    let (hub, tries) = hub_with(|_, _| {
        script(
            &[hello(PROTOCOL_VERSION), "not json".to_string()],
            "exec sleep 30",
        )
    });
    hub.ensure(&["noisy.ts".to_string()]);
    wait_until("fallback note", || hub.fallback_note("noisy.ts").is_some());
    let note = hub.fallback_note("noisy.ts").unwrap();
    assert!(note.contains("failed 3 times"), "{note}");
    assert!(note.contains("not a valid event"), "{note}");
    assert_eq!(attempts(&tries, "noisy.ts"), 3);
    stop(&hub, &["noisy.ts"]);
}

#[test]
fn stream_recovers_after_a_failed_connection() {
    let (hub, tries) = hub_with(|_, attempt| {
        if attempt == 1 {
            "exit 255".into()
        } else {
            script(
                &[hello(PROTOCOL_VERSION), snapshot(&["a"], false)],
                "exec sleep 30",
            )
        }
    });
    hub.ensure(&["flaky.ts".to_string()]);
    wait_until("live after retry", || hub.live("flaky.ts").is_some());
    assert_eq!(attempts(&tries, "flaky.ts"), 2);
    assert_eq!(hub.fallback_note("flaky.ts"), None);
    stop(&hub, &["flaky.ts"]);
}

#[test]
fn a_stalled_host_goes_stale_and_reconnects_while_another_stays_fresh() {
    let (hub, tries) = hub_with(|host, _| {
        if host == "stall.ts" {
            // One snapshot, then silence: no heartbeats.
            script(
                &[hello(PROTOCOL_VERSION), snapshot(&["s"], false)],
                "exec sleep 30",
            )
        } else {
            let beat = heartbeat();
            format!(
                "{} while :; do printf '%s\\n' '{beat}'; sleep 0.1; done",
                script(&[hello(PROTOCOL_VERSION), snapshot(&["h"], false)], "")
            )
        }
    });
    hub.ensure(&["stall.ts".to_string(), "healthy.ts".to_string()]);
    wait_until("both live", || {
        hub.live("stall.ts").is_some() && hub.live("healthy.ts").is_some()
    });
    wait_until("stalled host stale", || hub.live("stall.ts").is_none());
    assert!(hub.live("healthy.ts").is_some(), "other host stays fresh");
    wait_until("stalled host reconnects", || {
        attempts(&tries, "stall.ts") >= 2
    });
    assert!(hub.live("healthy.ts").is_some(), "other host still fresh");
    assert_eq!(attempts(&tries, "healthy.ts"), 1);
    stop(&hub, &["stall.ts", "healthy.ts"]);
}

#[test]
fn a_failed_sample_keeps_the_last_good_sessions_and_reports_it() {
    let (hub, _) = hub_with(|_, _| {
        script(
            &[
                hello(PROTOCOL_VERSION),
                snapshot(&["a"], false),
                snapshot(&[], true),
                heartbeat(),
            ],
            "exec sleep 30",
        )
    });
    hub.ensure(&["slow.ts".to_string()]);
    wait_until("failed sample reported", || {
        hub.live("slow.ts").is_some_and(|live| live.error.is_some())
    });
    assert_eq!(hub.live("slow.ts").unwrap().sessions.len(), 1);
    stop(&hub, &["slow.ts"]);
}

#[test]
fn a_failed_first_sample_after_reconnect_keeps_the_last_good_sessions() {
    let (hub, tries) = hub_with(|_, attempt| {
        if attempt == 1 {
            script(
                &[
                    hello(PROTOCOL_VERSION),
                    snapshot(&["a"], false),
                    heartbeat(),
                ],
                "sleep 0.3; exit 1",
            )
        } else {
            script(
                &[hello(PROTOCOL_VERSION), snapshot(&[], true), heartbeat()],
                "exec sleep 30",
            )
        }
    });
    hub.ensure(&["re.ts".to_string()]);
    wait_until("first live", || hub.live("re.ts").is_some());
    let first = hub.live("re.ts").unwrap().observed_at_unix_ms;
    wait_until("failed sample on the second connection", || {
        attempts(&tries, "re.ts") == 2 && hub.live("re.ts").is_some_and(|l| l.error.is_some())
    });
    let live = hub.live("re.ts").unwrap();
    assert_eq!(live.sessions.len(), 1, "the good records survive");
    assert_eq!(live.sessions[0].session_id, "a");
    assert!(
        live.observed_at_unix_ms >= first
            && live.observed_at.elapsed() >= Duration::from_millis(250),
        "the records keep their own observation time"
    );
    stop(&hub, &["re.ts"]);
}

#[test]
fn streams_that_die_after_a_snapshot_still_fall_back() {
    let (hub, tries) = hub_with(|_, _| {
        script(
            &[hello(PROTOCOL_VERSION), snapshot(&["a"], false)],
            "exit 1",
        )
    });
    hub.ensure(&["crashy.ts".to_string()]);
    wait_until("fallback note", || hub.fallback_note("crashy.ts").is_some());
    assert!(hub
        .fallback_note("crashy.ts")
        .unwrap()
        .contains("failed 3 times"));
    assert_eq!(attempts(&tries, "crashy.ts"), 3);
    stop(&hub, &["crashy.ts"]);
}

#[test]
fn cancelled_pipe_reads_end_while_a_writer_still_holds_the_pipe() {
    let (read_end, _write_end) = std::os::unix::net::UnixStream::pair().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut pipe = Cancellable::new(read_end, cancel.clone());
    let reader = std::thread::spawn(move || {
        let mut bytes = [0u8; 16];
        pipe.read(&mut bytes)
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(!reader.is_finished(), "an open, idle pipe blocks");
    cancel.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !reader.is_finished() {
        assert!(Instant::now() < deadline, "cancel must end the read");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reader.join().unwrap().unwrap(), 0);
}

#[test]
fn cancelled_pipes_still_return_data_already_waiting() {
    use std::io::Write;
    let (read_end, mut write_end) = std::os::unix::net::UnixStream::pair().unwrap();
    write_end.write_all(b"last words\n").unwrap();
    let cancel = Arc::new(AtomicBool::new(true));
    let mut pipe = Cancellable::new(read_end, cancel);
    let mut text = String::new();
    pipe.read_to_string(&mut text).unwrap();
    assert_eq!(text, "last words\n");
}

#[test]
fn the_stream_must_start_with_hello() {
    let (hub, _) = hub_with(|_, _| script(&[snapshot(&["a"], false)], "exec sleep 30"));
    hub.ensure(&["rude.ts".to_string()]);
    wait_until("fallback note", || hub.fallback_note("rude.ts").is_some());
    assert!(hub
        .fallback_note("rude.ts")
        .unwrap()
        .contains("did not start with hello"));
    assert!(hub.live("rude.ts").is_none());
    stop(&hub, &["rude.ts"]);
}

#[test]
fn bounded_lines_reject_oversize_input_without_buffering_it() {
    let mut reader = std::io::Cursor::new(b"short\nexactly-ten\n".to_vec());
    let mut line = Vec::new();
    assert_eq!(read_bounded_line(&mut reader, &mut line, 6), Ok(6));
    assert_eq!(line, b"short\n");
    line.clear();
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 6),
        Err(LineError::TooLong)
    );
    let mut reader = std::io::Cursor::new(b"tail-without-newline".to_vec());
    line.clear();
    assert_eq!(read_bounded_line(&mut reader, &mut line, 64), Ok(20));
    line.clear();
    assert_eq!(read_bounded_line(&mut reader, &mut line, 64), Ok(0));
}

#[test]
fn ssh_command_keeps_stdin_semantics_and_never_prompts() {
    let command = ssh_stream_command("kombiz@gpu-box.ts");
    assert_eq!(command.get_program(), "ssh");
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[0], "-T");
    assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
    assert!(args.windows(2).any(|w| w == ["-o", "ConnectTimeout=5"]));
    let dashdash = args.iter().position(|arg| arg == "--").expect("--");
    assert_eq!(args[dashdash + 1], "kombiz@gpu-box.ts");
    assert!(args[dashdash + 2].starts_with("sh -c '"));
    assert!(args[dashdash + 2].contains("exec taarof-remote stream"));
}

#[test]
fn unwanted_hosts_stop_and_kill_their_child() {
    let (hub, _) = hub_with(|_, _| {
        script(
            &[hello(PROTOCOL_VERSION), snapshot(&["a"], false)],
            "exec sleep 30",
        )
    });
    hub.ensure(&["a.ts".to_string(), "b.ts".to_string()]);
    wait_until("both live", || {
        hub.live("a.ts").is_some() && hub.live("b.ts").is_some()
    });
    hub.ensure(&["b.ts".to_string()]);
    let slot = hub.slot("a.ts").unwrap();
    wait_until("a stops", || !lock(&slot).running);
    assert!(hub.live("a.ts").is_none());
    assert!(hub.live("b.ts").is_some());
    stop(&hub, &["b.ts"]);
}
