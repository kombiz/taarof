//! Fire-and-forget child processes.
//!
//! `std::process::Child` has no `Drop` that reaps, so a helper we spawn and
//! never `wait()` stays in the process table as a zombie for the rest of the
//! session. GUI helpers make this worse than it sounds: `helium-browser` and
//! most editors hand the argument to an already-running instance and exit
//! immediately, so every link or file we open leaks one zombie.
//!
//! Anything spawned purely for its side effect goes through
//! [`spawn_and_reap`]. Callers that need the exit status keep using
//! `Command::status`/`output`/`try_wait` directly — those already reap.
//!
//! Note we deliberately do *not* set `SIGCHLD` to `SIG_IGN` to get automatic
//! reaping: that is process-wide and would make the `wait()`/`try_wait()` calls
//! in `tmux.rs` and `terminal/restore.rs` fail with `ECHILD`.

use std::io;
use std::process::Command;
use std::thread::JoinHandle;

/// A spawned fire-and-forget child plus the thread that will reap it.
///
/// Callers ignore this; dropping it detaches the reaper, which still reaps.
pub(crate) struct Reaped {
    // Both fields exist for the reaper thread's sake and for test
    // synchronization; production callers only ever discard the handle.
    #[cfg_attr(not(test), allow(dead_code))]
    pid: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    reaper: JoinHandle<()>,
}

impl Reaped {
    /// PID of the spawned child.
    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Block until the child has been reaped. Tests use this to synchronize;
    /// production callers fire and forget.
    #[cfg(test)]
    pub(crate) fn wait_for_reap(self) {
        let _ = self.reaper.join();
    }
}

/// Spawn a child for its side effect only and reap it once it exits.
pub(crate) fn spawn_and_reap(command: &mut Command) -> io::Result<Reaped> {
    // The one blessed spawn: the reaper thread below owns the wait().
    #[allow(clippy::disallowed_methods)]
    let mut child = command.spawn()?;
    let pid = child.id();
    let reaper = std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(Reaped { pid, reaper })
}

#[cfg(test)]
mod tests {
    use super::spawn_and_reap;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// Read the single-character state field from `/proc/<pid>/stat`, or `None`
    /// once the entry is gone. `comm` can contain spaces and parentheses, so the
    /// state is the first token after the final `)`.
    fn process_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().next()?.chars().next()
    }

    /// Poll until the child has either been reaped (entry gone) or has exited
    /// and gone zombie, so the assertion below is not racing the child's exit.
    fn settle(pid: u32) -> Option<char> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match process_state(pid) {
                None => return None,
                Some('Z') => return Some('Z'),
                Some(other) if Instant::now() >= deadline => return Some(other),
                Some(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn spawned_child_is_reaped_and_leaves_no_zombie() {
        let reaped = spawn_and_reap(&mut Command::new("true")).expect("spawn `true`");
        let pid = reaped.pid();
        reaped.wait_for_reap();

        assert_eq!(
            settle(pid),
            None,
            "pid {pid} is still in the process table after reaping (state Z = zombie)"
        );
    }
}
