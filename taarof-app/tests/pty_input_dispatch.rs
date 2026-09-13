//! Real unread-PTY regression: terminal outcomes forbid later remainder writes.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use taarof_app::pty_broker::{BrokeredPane, InputStatus, PtyBroker, SpawnSpec};

struct Fixture {
    pane: Arc<BrokeredPane>,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_publication_gate(false)
    }
    fn with_publication_gate(hold: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "taarof-input-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        if hold {
            fs::write(root.join("hold-publication"), b"").unwrap();
        }
        let script = r#"import os,sys,time,tty,select,pathlib
tty.setraw(0)
root=pathlib.Path(sys.argv[1])
print('INPUT_READY', flush=True)
while not (root/'go').exists(): time.sleep(.005)
out=bytearray()
while select.select([0],[],[],.3)[0]:
    chunk=os.read(0,65536)
    if not chunk: break
    out.extend(chunk)
with (root/'received.pending').open('wb') as received:
    if (root/'hold-publication').exists():
        (root/'publication-open').touch()
        while not (root/'release-publication').exists(): time.sleep(.005)
    received.write(out)
(root/'received.pending').replace(root/'received')
"#;
        let pane = Arc::new(
            PtyBroker::spawn(SpawnSpec {
                argv: vec![
                    "python3".into(),
                    "-u".into(),
                    "-c".into(),
                    script.into(),
                    root.to_string_lossy().into(),
                ],
                cwd: None,
                env: vec![],
                cols: 80,
                rows: 24,
            })
            .unwrap(),
        );
        let ready = pane.subscribe().expect("native subscription");
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains("INPUT_READY") {
            output.extend(ready.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        Self { pane, root }
    }
    fn start_reading(&self) {
        fs::write(self.root.join("go"), b"").unwrap();
    }
    fn received(&self) -> Vec<u8> {
        wait(|| self.root.join("received").exists());
        fs::read(self.root.join("received")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.pane.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < end, "condition deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn cancel_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn received_payload_is_published_only_after_the_write_completes() {
    let f = Fixture::with_publication_gate(true);
    let input = b"publication-boundary";
    f.pane.write_input(input).unwrap();
    f.start_reading();
    wait(|| f.root.join("publication-open").exists());
    // Hold the real child between opening its output and writing the payload.
    // Existence must never advertise a readable result during this interval.
    assert!(!f.root.join("received").exists());
    fs::write(f.root.join("release-publication"), b"").unwrap();
    assert_eq!(f.received(), input);
}

#[test]
fn partial_cancel_outcome_prevents_any_later_remainder_delivery() {
    let f = Fixture::new();
    let input = vec![b'x'; 65_536];
    let first = f
        .pane
        .submit_input(
            input.clone(),
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        )
        .unwrap();
    wait(|| first.written_bytes() > 0);
    let queued = f
        .pane
        .submit_input(
            vec![b'y'; 500],
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        )
        .unwrap();
    queued.cancel();
    assert_eq!(queued.wait_blocking().written_bytes, 0);
    first.cancel();
    let outcome = first.wait_blocking();
    assert_eq!(outcome.status, InputStatus::Cancelled);
    assert!(outcome.written_bytes > 0 && outcome.written_bytes < input.len());
    f.start_reading();
    assert_eq!(f.received(), input[..outcome.written_bytes]);
}

#[test]
fn deadlines_saturation_and_close_have_exact_bounded_outcomes() {
    let f = Fixture::new();
    let first = f
        .pane
        .submit_input(
            vec![b'x'; 65_536],
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        )
        .unwrap();
    wait(|| first.written_bytes() > 0);
    let expired = f
        .pane
        .submit_input(
            vec![b'z'; 100],
            Instant::now() + Duration::from_millis(30),
            cancel_flag(),
        )
        .unwrap();
    let outcome = expired.wait_blocking();
    assert_eq!(outcome.status, InputStatus::DeadlineExpired);
    assert_eq!(outcome.written_bytes, 0);
    let mut queued = Vec::new();
    loop {
        match f.pane.submit_input(
            vec![b'y'; 65_536],
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        ) {
            Ok(receipt) => {
                queued.push(receipt);
                assert!(queued.len() <= 16);
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                break;
            }
        }
    }
    assert!(!queued.is_empty());
    assert!(f
        .pane
        .submit_input(vec![0; 65_537], Instant::now(), cancel_flag())
        .is_err());
    f.pane.shutdown();
    assert_eq!(first.wait_blocking().status, InputStatus::Closed);
    for receipt in queued {
        let outcome = receipt.wait_blocking();
        assert_eq!(outcome.written_bytes, 0);
        assert_eq!(outcome.status, InputStatus::Closed);
    }
}

#[test]
fn native_and_web_inputs_use_one_ordered_writer() {
    let f = Fixture::new();
    let first = f
        .pane
        .submit_input(
            b"first".to_vec(),
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        )
        .unwrap();
    assert_eq!(first.wait_blocking().status, InputStatus::Delivered);
    f.pane.write_input(b"native").unwrap();
    let last = f
        .pane
        .submit_input(
            b"last".to_vec(),
            Instant::now() + Duration::from_secs(5),
            cancel_flag(),
        )
        .unwrap();
    assert_eq!(last.wait_blocking().status, InputStatus::Delivered);
    f.start_reading();
    assert_eq!(f.received(), b"firstnativelast");
}

#[test]
fn active_deadline_and_disconnect_cancel_only_the_unwritten_remainder() {
    for disconnect in [false, true] {
        let f = Fixture::new();
        let flag = cancel_flag();
        let receipt = f
            .pane
            .submit_input(
                vec![b'd'; 65_536],
                Instant::now() + Duration::from_millis(if disconnect { 5000 } else { 100 }),
                flag.clone(),
            )
            .unwrap();
        wait(|| receipt.written_bytes() > 0);
        if disconnect {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        let outcome = receipt.wait_blocking();
        assert_eq!(
            outcome.status,
            if disconnect {
                InputStatus::Cancelled
            } else {
                InputStatus::DeadlineExpired
            }
        );
        assert!(outcome.written_bytes > 0 && outcome.written_bytes < 65_536);
        f.start_reading();
        assert_eq!(f.received(), vec![b'd'; outcome.written_bytes]);
    }
}
