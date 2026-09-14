//! Inert real-PTY retained delivery bytes, RSS trend and byte-identity probe.
//! No installed app, runtime registry, credential or user output is accessed.
use sha2::{Digest, Sha256};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use taarof_app::pty_broker::{PtyBroker, SpawnSpec, NATIVE_QUEUE_BYTES, NATIVE_QUEUE_CHUNKS};
fn rss() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}
fn main() {
    let megabytes: usize = std::env::args()
        .nth(1)
        .unwrap_or("32".into())
        .parse()
        .unwrap();
    assert!((8..=128).contains(&megabytes));
    let total = megabytes * 1024 * 1024;
    let rss_before = rss();
    let (pane, receiver) = PtyBroker::spawn_with_native(SpawnSpec {
        argv: vec!["python3".into(), "-c".into(), format!(
            "import os\nb=b'x'*8192\nfor _ in range({}):\n p=b\n while p:\n  n=os.write(1,p);p=p[n:]", total / 8192)],
        cwd: None, env: vec![], cols: 80, rows: 24,
    }).unwrap();
    let started = Instant::now();
    let mut samples = Vec::new();
    let mut delivered = 0;
    let mut hasher = Sha256::new();
    for threshold in [0, total / 4, total / 2, total * 3 / 4] {
        while delivered < threshold {
            let bytes = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            delivered += bytes.len();
            hasher.update(&bytes);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while pane.subscriber_stats().native_waiting_writers == 0 {
            assert!(Instant::now() < deadline, "producer did not backpressure");
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats = pane.subscriber_stats();
        assert!(stats.native_queued_bytes <= NATIVE_QUEUE_BYTES);
        assert!(stats.native_queued_chunks <= NATIVE_QUEUE_CHUNKS);
        let cursor = pane.with_replay(|window| window.latest_seq());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(cursor, pane.with_replay(|window| window.latest_seq()));
        assert!(pane.try_exit_code().is_none());
        samples.push(
            serde_json::json!({"delivered_bytes":delivered, "retained":stats,
            "replay_bytes":pane.with_replay(|window|window.total_bytes()), "rss_kib":rss(),
            "producer_backpressured":true}),
        );
    }
    loop {
        match receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(bytes) => {
                delivered += bytes.len();
                hasher.update(&bytes);
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!("output drain failed: {error}"),
        }
    }
    assert!(!receiver.cancelled(), "natural EOF cannot be cancellation");
    assert_eq!(delivered, total);
    let mut expected = Sha256::new();
    for _ in 0..total / 8192 {
        expected.update([b'x'; 8192]);
    }
    let digest = format!("{:x}", hasher.finalize());
    assert_eq!(digest, format!("{:x}", expected.finalize()));
    assert!(pane.wait().unwrap().success());
    println!(
        "{}",
        serde_json::json!({"burst_bytes":total,"delivered_bytes":delivered,
        "sha256":digest,"samples":samples,"rss_before_kib":rss_before,
        "rss_after_kib":rss(),"elapsed_ms":started.elapsed().as_millis(),
        "native_queue_payload_cap":NATIVE_QUEUE_BYTES,
        "queue_entry_metadata_bytes":std::mem::size_of::<Vec<u8>>() * NATIVE_QUEUE_CHUNKS})
    );
}
