//! Bounded FIFO persistence for probe transitions only.
use super::DiagnosticRecord;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[cfg(not(test))]
pub(super) const CAPACITY: usize = 128;
#[cfg(not(test))]
pub(super) const SHUTDOWN_BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Drain {
    pub completed: bool,
    pub dropped: usize,
    pub failed: usize,
}

pub(crate) struct ProbeWriter {
    sender: Mutex<Option<mpsc::SyncSender<DiagnosticRecord>>>,
    done: Mutex<Option<mpsc::Receiver<()>>>,
    dropped: AtomicUsize,
    failed: Arc<AtomicUsize>,
}

impl ProbeWriter {
    pub(crate) fn start(
        capacity: usize,
        mut persist: impl FnMut(DiagnosticRecord) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (finished, done) = mpsc::channel();
        let failed = Arc::new(AtomicUsize::new(0));
        let worker_failed = failed.clone();
        let spawn = std::thread::Builder::new()
            .name("probe-diagnostics".into())
            .spawn(move || {
                for record in receiver {
                    if let Err(error) = persist(record) {
                        worker_failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("taarof: probe diagnostic persistence failed: {error}");
                    }
                }
                let _ = finished.send(());
            });
        if let Err(error) = spawn {
            eprintln!("taarof: could not start probe diagnostic writer: {error}");
        }
        Self {
            sender: Mutex::new(Some(sender)),
            done: Mutex::new(Some(done)),
            dropped: AtomicUsize::new(0),
            failed,
        }
    }

    /// Never acquire a sink lock or wait for queue space. Contended admission,
    /// a full queue, shutdown, and a disconnected worker all reject the newest
    /// record. Loss remains observable even after the sink recovers.
    pub(crate) fn enqueue(&self, record: DiagnosticRecord) -> bool {
        let accepted = self.sender.try_lock().ok().is_some_and(|sender| {
            sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(record).is_ok())
        });
        if !accepted {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        accepted
    }

    /// Closing admission disconnects the FIFO once the brief sender lock is
    /// released. The worker then drains accepted records. A stalled sink is
    /// detached after the deadline; completion means sink calls finished, not
    /// that history's independent durable flush has succeeded.
    pub(crate) fn shutdown(&self, budget: Duration) -> Option<Drain> {
        let receiver = self.done.lock().ok()?.take()?;
        self.sender.lock().ok()?.take();
        let completed = receiver.recv_timeout(budget).is_ok();
        Some(Drain {
            completed,
            dropped: self.dropped.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticJournal, DiagnosticRetention};

    fn record(n: usize) -> DiagnosticRecord {
        super::super::make_record(
            super::super::DiagnosticLevel::Info,
            "lifecycle",
            "runtime",
            "probe-recovered",
            format!("record {n}"),
            None,
        )
    }

    #[test]
    fn probe_writer_stalled_sink_does_not_block_admission_and_preserves_fifo_after_loss() {
        let (entered, waiting) = mpsc::channel();
        let (release, barrier) = mpsc::channel();
        let (written, observed) = mpsc::channel();
        let mut first = true;
        let writer = ProbeWriter::start(2, move |record| {
            if first {
                first = false;
                entered.send(()).unwrap();
                barrier.recv().unwrap();
            }
            written.send(record.message).unwrap();
            Ok(())
        });
        assert!(writer.enqueue(record(0)));
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(writer.enqueue(record(1)));
        assert!(writer.enqueue(record(2)));
        assert!(!writer.enqueue(record(3)));
        release.send(()).unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "record 0"
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "record 1"
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "record 2"
        );
        assert!(writer.enqueue(record(4)));
        assert_eq!(
            writer.shutdown(Duration::from_secs(2)),
            Some(Drain {
                completed: true,
                dropped: 1,
                failed: 0
            })
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            "record 4"
        );
        assert!(!writer.enqueue(record(5)));
        assert_eq!(writer.dropped.load(Ordering::Relaxed), 2);
        assert_eq!(writer.shutdown(Duration::ZERO), None);
    }

    #[test]
    fn probe_writer_shutdown_timeout_does_not_wait_for_stalled_sink() {
        let (entered, waiting) = mpsc::channel();
        let (release, barrier) = mpsc::channel();
        let (finished, observed) = mpsc::channel();
        let writer = ProbeWriter::start(1, move |_| {
            entered.send(()).unwrap();
            barrier.recv().unwrap();
            finished.send(()).unwrap();
            Ok(())
        });
        assert!(writer.enqueue(record(0)));
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            writer.shutdown(Duration::ZERO),
            Some(Drain {
                completed: false,
                dropped: 0,
                failed: 0
            })
        );
        assert!(!writer.enqueue(record(1)));
        release.send(()).unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn ledger_shutdown_report_rejected_by_full_writer_remains_visible_and_incomplete() {
        let (entered, waiting) = mpsc::channel();
        let (release, barrier) = mpsc::channel();
        let mut first = true;
        let writer = ProbeWriter::start(1, move |_| {
            if first {
                first = false;
                entered.send(()).unwrap();
                barrier.recv().unwrap();
            }
            Ok(())
        });
        assert!(writer.enqueue(record(0)));
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(writer.enqueue(record(1)));
        let reports = std::cell::RefCell::new(Vec::new());
        let report = |message: &str| reports.borrow_mut().push(message.to_string());
        super::super::report_ledger_shutdown_with(
            "work ledger persistence incomplete at shutdown: test",
            &|record| writer.enqueue(record),
            &report,
        );
        assert_eq!(
            reports.borrow()[0],
            "work ledger persistence incomplete at shutdown: test"
        );
        assert!(reports.borrow()[1].contains("rejected admission"));
        release.send(()).unwrap();
        super::super::drain_probe_writer_with(&writer, Duration::from_secs(2), &report);
        assert!(reports.borrow()[2].contains("drained=true, dropped=1"));
    }

    #[test]
    fn probe_writer_reports_actual_sink_failures() {
        let writer = ProbeWriter::start(2, |_| Err("test sink failed".into()));
        assert!(writer.enqueue(record(0)));
        assert_eq!(
            writer.shutdown(Duration::from_secs(2)),
            Some(Drain {
                completed: true,
                dropped: 0,
                failed: 1
            })
        );
    }

    #[test]
    fn probe_writer_uses_existing_private_journal_rotation() {
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::tests::unique_temp_dir("probe-writer-rotation");
        let log = dir.join("journal.jsonl");
        let archive = dir.join("journal.jsonl.1");
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log.clone()),
            Some(archive.clone()),
            DiagnosticRetention {
                recent_record_limit: 64,
                log_max_bytes: 1,
                archive_count: 1,
            },
        );
        let writer = ProbeWriter::start(2, move |record| journal.record_reporting(record));
        assert!(writer.enqueue(record(0)));
        assert!(writer.enqueue(record(1)));
        assert_eq!(
            writer.shutdown(Duration::from_secs(2)),
            Some(Drain {
                completed: true,
                dropped: 0,
                failed: 0
            })
        );
        let latest: DiagnosticRecord =
            serde_json::from_str(std::fs::read_to_string(&log).unwrap().trim()).unwrap();
        let previous: DiagnosticRecord =
            serde_json::from_str(std::fs::read_to_string(&archive).unwrap().trim()).unwrap();
        assert_eq!(latest.message, "record 1");
        assert_eq!(previous.message, "record 0");
        for path in [&log, &archive] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
