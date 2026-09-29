//! Child-process construction and fire-and-forget reaping.
//!
//! Every child taarof starts is built by [`command`], which applies the
//! child-environment sanitizer before the caller adds anything else. Clippy
//! rejects a bare `std::process::Command::new` everywhere else
//! (`taarof-app/clippy.toml`), so a new helper cannot silently forward an
//! ambient Infisical token to `gh`, `git`, `coder`, or any other child.
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

use std::ffi::OsStr;
use std::io;
use std::process::Command;
use std::thread::JoinHandle;

/// Build a child command whose inherited environment has already passed
/// [`crate::child_env::prepare_child_command`].
///
/// Callers may still add their own variables afterward; the sanitizer only
/// guarantees that ambient tokens and the internal reload marker are not
/// inherited. Callers with an explicit override list (PTY panes) run
/// `prepare_child_command` again with it so overrides are filtered too.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    // The one blessed constructor: every other site goes through here.
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(program);
    crate::child_env::prepare_child_command(&mut command, &[]);
    command
}

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
    use super::{command, spawn_and_reap};
    use std::collections::HashMap;
    use std::ffi::OsString;
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
        let reaped = spawn_and_reap(&mut command("true")).expect("spawn `true`");
        let pid = reaped.pid();
        reaped.wait_for_reap();

        assert_eq!(
            settle(pid),
            None,
            "pid {pid} is still in the process table after reaping (state Z = zombie)"
        );
    }

    fn envs(command: &std::process::Command) -> HashMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsString::from)))
            .collect()
    }

    #[test]
    fn constructor_strips_ambient_tokens_and_reload_marker() {
        let env = envs(&command("true"));
        for name in [
            "INFISICAL_TOKEN",
            "INFISICAL_SERVICE_TOKEN",
            crate::child_env::RELOAD_RESUME_ENV,
        ] {
            assert_eq!(env.get(&OsString::from(name)), Some(&None), "{name}");
        }
    }

    /// Every `disallowed_methods` allowance in `src/` must cover only a spawn
    /// of an already-built command, never its construction, so a bare
    /// `Command::new` cannot hide under a reaping opt-out. The one exception is
    /// the blessed constructor above.
    #[test]
    fn disallowed_method_allowances_never_cover_command_construction() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut violations = Vec::new();
        let mut pending = vec![src];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).expect("read source");
                    violations.extend(allowance_violations(&path, &text));
                }
            }
        }
        assert!(violations.is_empty(), "{violations:#?}");
    }

    #[test]
    fn allowance_scan_rejects_construction_under_the_allowance() {
        let path = std::path::Path::new("fixture.rs");
        let bad = "#[allow(clippy::disallowed_methods)]\nlet child = Command::new(\"x\")\n    .spawn();\n";
        assert_eq!(allowance_violations(path, bad).len(), 1);
        let wrapped = "#[allow(clippy::disallowed_methods)]\nlet child = crate::child_process::command(\"x\")\n    .spawn();\n";
        assert_eq!(allowance_violations(path, wrapped).len(), 1);
        let item = "#[allow(clippy::disallowed_methods)]\nfn helper() {\n    let _ = Command::new(\"x\");\n}\n";
        assert_eq!(allowance_violations(path, item).len(), 1);
        let module = "#![allow(clippy::disallowed_methods)]\n";
        assert_eq!(allowance_violations(path, module).len(), 1);
        let good = "// Reaped below.\n#[allow(clippy::disallowed_methods)]\nlet mut child = command\n    .spawn()?;\n";
        assert!(allowance_violations(path, good).is_empty());
    }

    /// Return one entry per allowance whose covered statement is not a bare
    /// spawn of an existing command.
    fn allowance_violations(path: &std::path::Path, text: &str) -> Vec<String> {
        let blessed = path.ends_with("src/child_process.rs");
        let lines: Vec<&str> = text.lines().collect();
        let mut violations = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("#![allow(") && trimmed.contains("clippy::disallowed_methods") {
                violations.push(format!(
                    "{}:{}: module-wide allowance",
                    path.display(),
                    index + 1
                ));
                continue;
            }
            if !(trimmed.starts_with("#[allow(") && trimmed.contains("clippy::disallowed_methods"))
            {
                continue;
            }
            // The covered statement runs up to and including the line that
            // spawns; allow a short builder chain before it.
            let covered: Vec<&str> = lines[index + 1..]
                .iter()
                .take(3)
                .map(|line| line.trim())
                .collect();
            let Some(end) = covered.iter().position(|line| line.contains(".spawn()")) else {
                if blessed
                    && covered
                        .first()
                        .is_some_and(|line| *line == "let mut command = Command::new(program);")
                {
                    continue;
                }
                violations.push(format!(
                    "{}:{}: allowance does not cover a spawn",
                    path.display(),
                    index + 1
                ));
                continue;
            };
            if covered[..=end]
                .iter()
                .any(|line| line.contains("Command::new(") || line.contains("command("))
            {
                violations.push(format!(
                    "{}:{}: allowance covers command construction",
                    path.display(),
                    index + 1
                ));
            }
        }
        violations
    }
}
