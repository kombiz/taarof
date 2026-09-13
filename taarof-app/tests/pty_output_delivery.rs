//! Inert real-PTY regressions for bounded subscriber ownership and native drain.
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use taarof_app::pty_broker::{
    BrokeredPane, NativeSubscription, PtyBroker, SpawnSpec, MAX_NATIVE_SUBSCRIBERS,
    MAX_WEB_OBSERVERS, NATIVE_QUEUE_BYTES, NATIVE_QUEUE_CHUNKS,
};

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: None,
        env: vec![],
        cols: 80,
        rows: 24,
    }
}
fn wait_full(pane: &BrokeredPane) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pane.subscriber_stats().native_waiting_writers == 0 {
        assert!(
            Instant::now() < deadline,
            "native writer did not backpressure"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn drain(receiver: NativeSubscription) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(bytes) => output.extend(bytes),
            Err(mpsc::RecvTimeoutError::Disconnected) => return output,
            Err(error) => panic!("native drain stalled: {error}"),
        }
    }
}
#[test]
fn full_native_queue_keeps_queries_checkpoint_sibling_and_close_usable() {
    let (pane, receiver) =
        PtyBroker::spawn_with_native(spec("head -c 16777216 /dev/zero; sleep 60")).unwrap();
    let pane = Arc::new(pane);
    let additional: Vec<_> = (1..MAX_NATIVE_SUBSCRIBERS)
        .map(|_| pane.subscribe().unwrap())
        .collect();
    wait_full(&pane);
    let stats = pane.subscriber_stats();
    assert_eq!(stats.native_subscribers, MAX_NATIVE_SUBSCRIBERS);
    assert!(stats.native_queued_bytes <= MAX_NATIVE_SUBSCRIBERS * NATIVE_QUEUE_BYTES);
    assert!(stats.native_queued_chunks <= MAX_NATIVE_SUBSCRIBERS * NATIVE_QUEUE_CHUNKS);
    let query = pane.clone();
    let (done, result) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let cursor = query.with_replay(|replay| replay.latest_seq());
        query.bounded_checkpoint(256 * 1024).unwrap();
        query.resize(100, 30).unwrap();
        done.send(cursor).unwrap();
    });
    let queried = result.recv_timeout(Duration::from_secs(1));
    // Release the queue before assertions so a failing query probe cleans up.
    if queried.is_err() {
        drop(receiver);
        pane.shutdown();
        thread.join().unwrap();
        panic!("query blocked behind native capacity wait");
    }
    thread.join().unwrap();
    let (sibling, sibling_rx) = PtyBroker::spawn_with_native(spec("printf SIBLING_OK")).unwrap();
    assert_eq!(drain(sibling_rx), b"SIBLING_OK");
    sibling.shutdown();
    let started = Instant::now();
    pane.shutdown();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(pane.subscriber_stats().native_queued_bytes, 0);
    assert!(receiver.recv_timeout(Duration::ZERO).is_err());
    drop(additional);
}
#[test]
fn natural_fast_exit_preserves_every_byte_after_stalled_receiver_resumes() {
    let count = 6 * 1024 * 1024;
    let (pane, receiver) = PtyBroker::spawn_with_native(spec(&format!(
        "head -c {count} /dev/zero; printf FINAL_SENTINEL"
    )))
    .unwrap();
    wait_full(&pane);
    assert!(
        pane.try_exit_code().is_none(),
        "producer must remain backpressured"
    );
    let output = drain(receiver);
    assert_eq!(output.len(), count + b"FINAL_SENTINEL".len());
    assert!(output[..count].iter().all(|byte| *byte == 0));
    assert_eq!(&output[count..], b"FINAL_SENTINEL");
    pane.shutdown();
}
#[test]
fn native_and_web_admission_caps_are_released_on_receiver_drop() {
    let pane = PtyBroker::spawn(spec("sleep 60")).unwrap();
    let native: Vec<_> = (0..MAX_NATIVE_SUBSCRIBERS)
        .map(|_| pane.subscribe().unwrap())
        .collect();
    assert!(pane.subscribe().is_err());
    let web: Vec<_> = (0..MAX_WEB_OBSERVERS)
        .map(|_| pane.observe_output().unwrap())
        .collect();
    assert!(pane.observe_output().is_err());
    assert_eq!(
        pane.subscriber_stats().native_subscribers,
        MAX_NATIVE_SUBSCRIBERS
    );
    assert_eq!(pane.subscriber_stats().web_observers, MAX_WEB_OBSERVERS);
    drop(native);
    drop(web);
    assert!(pane.subscribe().is_ok());
    assert!(pane.observe_output().is_ok());
    pane.shutdown();
}
#[test]
fn late_native_attach_rejects_evicted_history_instead_of_delivering_a_suffix() {
    let pane = PtyBroker::spawn(spec("head -c 6291456 /dev/zero")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let observer = pane.observe_output().unwrap();
    while !observer.closed() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(pane.subscribe().is_err());
    // Coalesced observers retained no payload queue despite the entire burst.
    assert_eq!(pane.subscriber_stats().native_queued_bytes, 0);
    pane.shutdown();
}
