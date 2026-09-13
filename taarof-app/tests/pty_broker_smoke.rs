//! Fake-child smoke tests for the broker-owned Unix PTY.
//!
//! These assert the broker's central invariant: it is the sole reader of the
//! child PTY master, and every chunk it reads is fanned out — in identical
//! order — to the local presentation subscriber, the bounded replay window, and
//! the headless terminal-state model. They spawn real (but tiny and
//! deterministic) child processes over a real PTY, so they run headlessly
//! without GTK.

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use taarof_app::pty_broker::NativeSubscription;

use taarof_app::pty_broker::{PtyBroker, SpawnSpec};

/// Drain a presentation subscriber to EOF. The broker closes its senders when
/// the child exits, so `recv` reports `Disconnected` and the loop ends. A
/// timeout is a test failure, not a normal exit, so a hung child cannot wedge
/// the suite.
fn drain_to_eof(rx: NativeSubscription) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for broker output"),
        }
    }
    out
}

fn spawn_sh(command: &str, cols: u16, rows: u16) -> taarof_app::pty_broker::BrokeredPane {
    PtyBroker::spawn(SpawnSpec {
        argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
        cwd: None,
        env: Vec::new(),
        cols,
        rows,
    })
    .expect("broker should allocate a PTY and spawn the fake child")
}

#[test]
fn fake_child_fans_identical_ordered_bytes_to_presentation_replay_and_model() {
    // A deterministic marker the child writes exactly once. `printf` (no
    // newline) keeps the byte stream trivial to reason about.
    let marker = "ALPHA-BROKER-42";
    let pane = spawn_sh(&format!("printf '{marker}'"), 80, 24);

    // Subscribe as the "local presentation" consumer would.
    let rx = pane.subscribe().expect("native subscription");

    // Collect everything the presentation subscriber sees.
    let presentation = drain_to_eof(rx);

    // The replay window must hold the exact same ordered bytes the presentation
    // subscriber received: one is the live fan-out, the other is the retained
    // copy, and they are fed from the same read in the same critical section.
    let replayed = pane.with_replay(|window| {
        window
            .frames()
            .iter()
            .flat_map(|frame| frame.payload.clone())
            .collect::<Vec<u8>>()
    });

    assert_eq!(
        presentation, replayed,
        "presentation and replay must receive identical ordered bytes"
    );
    assert!(
        presentation
            .windows(marker.len())
            .any(|w| w == marker.as_bytes()),
        "presentation stream must contain the child's marker: {presentation:?}"
    );

    // The state model is fed the same bytes, so its projection must reflect the
    // marker on the first row.
    let visible = pane.with_model(|model| model.projection().unwrap().visible_text.join("\n"));
    assert!(
        visible.contains(marker),
        "state model must reflect the same bytes; visible text was {visible:?}"
    );
}

#[test]
fn pty_child_does_not_receive_ambient_or_explicit_infisical_tokens() {
    let pane = PtyBroker::spawn(SpawnSpec {
        argv: vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "if [ \"${INFISICAL_TOKEN+x}${INFISICAL_SERVICE_TOKEN+x}\" = \"\" ]; then printf 'TOKENS_ABSENT'; else printf 'TOKENS_PRESENT'; fi".to_string(),
        ],
        cwd: None,
        // Prove a caller-provided override cannot bypass the same boundary.
        env: vec![
            ("INFISICAL_TOKEN".into(), "synthetic-override".into()),
            ("INFISICAL_SERVICE_TOKEN".into(), "synthetic-service".into()),
        ],
        cols: 80,
        rows: 24,
    })
    .expect("broker should spawn the token-boundary probe");

    let output = String::from_utf8_lossy(&drain_to_eof(
        pane.subscribe().expect("native subscription"),
    ))
    .into_owned();
    assert!(
        output.contains("TOKENS_ABSENT"),
        "PTY child must not receive ambient Infisical tokens; saw {output:?}"
    );
    assert!(!output.contains("TOKENS_PRESENT"));
}

#[test]
fn write_input_reaches_child_over_the_pty() {
    // The child reads one line from its PTY stdin and echoes it back with a
    // recognizable prefix. This exercises `write_input` end to end.
    let pane = spawn_sh("IFS= read -r line; printf 'GOT:%s\\n' \"$line\"", 80, 24);
    let rx = pane.subscribe().expect("native subscription");

    pane.write_input(b"hello-input\n")
        .expect("write_input should deliver bytes to the PTY master");

    let output = String::from_utf8_lossy(&drain_to_eof(rx)).into_owned();
    assert!(
        output.contains("GOT:hello-input"),
        "child should observe written input; saw {output:?}"
    );
}

#[test]
fn spawn_applies_initial_window_size_and_resize_updates_the_model() {
    // `stty size` prints "rows cols" for its controlling tty, proving the
    // broker's initial winsize propagated to the child PTY.
    let pane = spawn_sh("stty size", 80, 24);
    let rx = pane.subscribe().expect("native subscription");
    let output = String::from_utf8_lossy(&drain_to_eof(rx)).into_owned();
    assert!(
        output.contains("24 80"),
        "child tty should report the spawned 24x80 size; saw {output:?}"
    );

    // Resize must retarget the model dimensions (and the PTY ioctl must succeed).
    let long_lived = spawn_sh("IFS= read -r _", 80, 24);
    long_lived
        .resize(120, 40)
        .expect("resize should succeed on a live PTY");
    let (cols, rows) = long_lived.with_model(|model| {
        let projection = model.projection().unwrap();
        (projection.cols, projection.rows)
    });
    assert_eq!(
        (cols, rows),
        (120, 40),
        "resize must update the state model"
    );
    long_lived
        .write_input(b"\n")
        .expect("unblock the child so it exits cleanly");
    drain_to_eof(long_lived.subscribe().expect("native subscription"));
}

/// Poll the broker's non-blocking exit code (the exact call the GTK exit poller
/// uses) until the child has been reaped.
fn wait_for_exit_code(pane: &taarof_app::pty_broker::BrokeredPane) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(code) = pane.try_exit_code() {
            return code;
        }
        assert!(Instant::now() < deadline, "child did not exit in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn signal_killed_child_surfaces_128_plus_signal_not_success() {
    // A child that kills itself with SIGKILL (9) has no exit code. The broker
    // must surface 128+9=137 (shell convention), never a false 0, so a crash or
    // kill stays a failure through the exit path (workspace Errored status,
    // truthful command_exited).
    let pane = spawn_sh("kill -9 $$", 80, 24);
    let code = wait_for_exit_code(&pane);
    assert_eq!(code, 137, "SIGKILL child must surface 128+9, got {code}");
}

#[test]
fn combining_overflow_preserves_exact_native_and_replay_bytes_but_refuses_checkpoint() {
    let pane = PtyBroker::spawn(SpawnSpec {
        argv: vec!["python3".into(), "-c".into(),
            "import os\nb = b'a' + b'\\xcc\\x81' * 262144\nwhile b:\n n = os.write(1, b); b = b[n:]".into()],
        cwd: None,
        env: vec![],
        cols: 4,
        rows: 2,
    }).unwrap();
    let native = drain_to_eof(pane.subscribe().unwrap());
    let expected = [b"a".as_slice(), "\u{301}".repeat(262144).as_bytes()].concat();
    assert_eq!(
        native, expected,
        "native delivery must be byte-identical despite model overflow"
    );
    let replay = pane.with_replay(|window| {
        window
            .frames()
            .iter()
            .flat_map(|frame| frame.payload.iter().copied())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        replay, expected,
        "replay is not model text and must never be truncated"
    );
    let cursor = pane.with_replay(|window| window.latest_seq());
    let error = pane.bounded_checkpoint(256 * 1024).unwrap_err();
    assert!(error
        .get_ref()
        .unwrap()
        .is::<taarof_app::pty_broker::screen::ModelDegraded>());
    assert!(
        pane.checkpoint().is_err(),
        "unbudgeted caller must also refuse degraded state"
    );
    assert_eq!(pane.with_replay(|window| window.latest_seq()), cursor);
    assert!(pane.with_model(|model| model.retained_text_bytes().unwrap()) <= 16 * 1024);
    assert!(pane.wait().unwrap().success());
}
