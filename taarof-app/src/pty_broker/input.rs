//! One bounded, ordered nonblocking writer per pane. Completion counts kernel
//! acceptance only; it cannot claim the terminal child consumed those bytes.
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

pub const INPUT_MAX_BYTES: usize = 65_536;
const QUEUE_JOBS: usize = 16;
const POLL_MS: i32 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputStatus {
    Delivered,
    Cancelled,
    DeadlineExpired,
    Closed,
    WriteFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct InputOutcome {
    pub requested_bytes: usize,
    pub written_bytes: usize,
    pub status: InputStatus,
}

struct Progress {
    outcome: InputOutcome,
    reply: Option<oneshot::Sender<InputOutcome>>,
    ready: Arc<Condvar>,
}

impl Progress {
    fn finish(&mut self, status: InputStatus) {
        if let Some(reply) = self.reply.take() {
            self.outcome.status = status;
            let _ = reply.send(self.outcome.clone());
            self.ready.notify_all();
        }
    }
}

/// Dropping a receipt cancels its unwritten remainder. Cancellation and each
/// nonblocking write hold the same short lock: after a terminal outcome is
/// returned the writer cannot dispatch any further bytes from that request.
pub struct InputReceipt {
    progress: Arc<Mutex<Progress>>,
    reply: Option<oneshot::Receiver<InputOutcome>>,
    deadline: Option<Instant>,
    ready: Arc<Condvar>,
}

impl InputReceipt {
    pub fn cancel(&self) {
        self.progress.lock().unwrap().finish(InputStatus::Cancelled);
    }

    pub fn written_bytes(&self) -> usize {
        self.progress.lock().unwrap().outcome.written_bytes
    }

    pub async fn wait(mut self) -> InputOutcome {
        let mut reply = self.reply.take().unwrap();
        if let Some(deadline) = self.deadline {
            tokio::select! {
                result = &mut reply => return result.expect("writer always completes admitted input"),
                _ = tokio::time::sleep_until(deadline.into()) => {
                    self.progress.lock().unwrap().finish(InputStatus::DeadlineExpired);
                }
            }
        }
        reply.await.expect("writer always completes admitted input")
    }

    pub fn wait_blocking(self) -> InputOutcome {
        let mut progress = self.progress.lock().unwrap();
        while progress.reply.is_some() {
            if let Some(deadline) = self.deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    progress.finish(InputStatus::DeadlineExpired);
                    break;
                }
                progress = self.ready.wait_timeout(progress, remaining).unwrap().0;
            } else {
                progress = self.ready.wait(progress).unwrap();
            }
        }
        progress.outcome.clone()
    }
}

impl Drop for InputReceipt {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Job {
    bytes: Vec<u8>,
    progress: Arc<Mutex<Progress>>,
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.progress.lock().unwrap().finish(InputStatus::Closed);
    }
}

pub(super) struct InputWriter {
    queue: mpsc::SyncSender<Job>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl InputWriter {
    pub fn new(master: Arc<OwnedFd>) -> Self {
        let (queue, jobs) = mpsc::sync_channel::<Job>(QUEUE_JOBS);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match jobs.recv_timeout(Duration::from_millis(POLL_MS as u64)) {
                    Ok(job) => write_job(master.as_raw_fd(), &job, &stop),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            for job in jobs.try_iter() {
                job.progress.lock().unwrap().finish(InputStatus::Closed);
            }
        });
        Self {
            queue,
            stopped,
            worker: Some(worker),
        }
    }

    pub fn submit(
        &self,
        bytes: Vec<u8>,
        deadline: Option<Instant>,
        cancelled: Arc<AtomicBool>,
        wait_for_space: bool,
    ) -> io::Result<InputReceipt> {
        if bytes.len() > INPUT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY input exceeds 65536 bytes",
            ));
        }
        if self.stopped.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY input is closed or cancelled",
            ));
        }
        let (reply, receiver) = oneshot::channel();
        let ready = Arc::new(Condvar::new());
        let progress = Arc::new(Mutex::new(Progress {
            outcome: InputOutcome {
                requested_bytes: bytes.len(),
                written_bytes: 0,
                status: InputStatus::Delivered,
            },
            reply: Some(reply),
            ready: ready.clone(),
        }));
        let job = Job {
            bytes,
            progress: progress.clone(),
            deadline,
            cancelled,
        };
        if wait_for_space {
            self.queue.send(job).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "PTY input writer closed")
            })?;
        } else {
            self.queue.try_send(job).map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "PTY input writer closed")
                }
            })?;
        }
        Ok(InputReceipt {
            progress,
            reply: Some(receiver),
            deadline,
            ready,
        })
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn write_job(fd: i32, job: &Job, stopped: &AtomicBool) {
    loop {
        {
            let mut progress = job.progress.lock().unwrap();
            if progress.reply.is_none() {
                return;
            }
            let status = if stopped.load(Ordering::Acquire) {
                Some(InputStatus::Closed)
            } else if job.cancelled.load(Ordering::Acquire) {
                Some(InputStatus::Cancelled)
            } else if job
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                Some(InputStatus::DeadlineExpired)
            } else {
                None
            };
            if let Some(status) = status {
                progress.finish(status);
                return;
            }
            let offset = progress.outcome.written_bytes;
            if offset == job.bytes.len() {
                progress.finish(InputStatus::Delivered);
                return;
            }
            // A small nonblocking syscall bounds cancellation's lock wait.
            let chunk = &job.bytes[offset..job.bytes.len().min(offset + 4096)];
            let written = unsafe { libc::write(fd, chunk.as_ptr().cast(), chunk.len()) };
            if written > 0 {
                progress.outcome.written_bytes += written as usize;
                continue;
            }
            if written == 0 {
                progress.finish(InputStatus::WriteFailed);
                return;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock {
                progress.finish(InputStatus::WriteFailed);
                return;
            }
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, POLL_MS) };
        if (result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
            || poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            job.progress.lock().unwrap().finish(InputStatus::Closed);
            return;
        }
    }
}
