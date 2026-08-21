//! Child-process environment boundary.
//!
//! taarof normally inherits its desktop launch environment and terminal shells
//! inherit taarof's environment in turn. That is useful for ordinary session
//! configuration, but an ambient Infisical access/service token must not become
//! a shared credential in every pane. Consumers that need Infisical should mint
//! their own token through the configured machine-identity/broker path.

use std::ffi::CStr;
use std::process::Command;

unsafe extern "C" {
    static mut environ: *mut *mut libc::c_char;
}

/// Token-shaped Infisical credentials that must never cross taarof's process or
/// PTY boundary. Machine-identity inputs are intentionally not listed: explicit
/// consumers may use those to mint their own short-lived token.
const AMBIENT_INFISICAL_TOKEN_VARS: &[&str] = &["INFISICAL_TOKEN", "INFISICAL_SERVICE_TOKEN"];
pub(crate) const RELOAD_RESUME_ENV: &str = "TAAROF_RELOAD_RESUME_AGENTS";

#[derive(Debug)]
pub(super) struct StartupEnvironment {
    resume_agents_after_reload: bool,
}

impl StartupEnvironment {
    fn from_reload_marker(resume_agents_after_reload: bool) -> Self {
        Self {
            resume_agents_after_reload,
        }
    }

    pub(super) fn take_resume_agents_after_reload(&mut self) -> bool {
        std::mem::take(&mut self.resume_agents_after_reload)
    }
}

pub(crate) fn is_ambient_infisical_token(name: &str) -> bool {
    AMBIENT_INFISICAL_TOKEN_VARS.contains(&name)
}

/// Clear inherited tokens and capture the internal reload marker before GTK,
/// background workers, or terminals start.
///
/// # Safety
///
/// The caller must invoke this exactly once, as the first operation in `run()`,
/// while taarof is still single-threaded. A process-wide guard rejects re-entry.
pub(super) unsafe fn initialize_startup_environment() -> StartupEnvironment {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    assert!(
        INITIALIZED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "startup environment was initialized more than once"
    );

    let resume_agents_after_reload = std::env::var_os(RELOAD_RESUME_ENV).is_some();

    // SAFETY: `run()` calls this private startup boundary before GTK
    // initialization or any taarof thread is created, so no concurrent
    // environment access exists yet.
    unsafe {
        for name in AMBIENT_INFISICAL_TOKEN_VARS {
            // Wipe every matching entry (not just `getenv`'s first match) before
            // `unsetenv`: `execve` permits duplicate names, and Linux may expose
            // original environment memory through `/proc/<pid>/environ` afterward.
            wipe_environment_entries(environ, name);
            std::env::remove_var(name);
        }
        std::env::remove_var(RELOAD_RESUME_ENV);
    }

    StartupEnvironment::from_reload_marker(resume_agents_after_reload)
}

/// Zero the value bytes of every `name=...` entry in a libc environment vector.
/// The key and `=` remain intact so we never resize or relocate launcher-owned
/// memory; callers remove the logical variable separately with `unsetenv`.
unsafe fn wipe_environment_entries(mut entries: *mut *mut libc::c_char, name: &str) {
    if entries.is_null() {
        return;
    }
    let prefix = format!("{name}=");
    loop {
        // SAFETY: caller supplies a conventional NULL-terminated `environ`
        // vector and calls before concurrent mutation.
        let entry_ptr = unsafe { *entries };
        if entry_ptr.is_null() {
            break;
        }
        // SAFETY: each environment entry is a NUL-terminated C string.
        let entry = unsafe { CStr::from_ptr(entry_ptr) }.to_bytes();
        if entry.starts_with(prefix.as_bytes()) {
            let value_len = entry.len().saturating_sub(prefix.len());
            // SAFETY: only the existing value bytes are overwritten in place;
            // the allocation, key, separator, and terminal NUL are untouched.
            unsafe {
                std::ptr::write_bytes(entry_ptr.add(prefix.len()).cast::<u8>(), 0, value_len)
            };
        }
        // SAFETY: advance within the NULL-terminated pointer vector.
        entries = unsafe { entries.add(1) };
    }
}

/// Apply the explicit environment for a child while enforcing the token denylist
/// both against inherited process state and caller-provided overrides.
pub(crate) fn prepare_child_command(command: &mut Command, overrides: &[(String, String)]) {
    for name in AMBIENT_INFISICAL_TOKEN_VARS {
        command.env_remove(name);
    }
    command.env_remove(RELOAD_RESUME_ENV);
    for (name, value) in overrides {
        if !is_ambient_infisical_token(name) && name != RELOAD_RESUME_ENV {
            command.env(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    #[test]
    fn reload_resume_intent_is_consumed_once() {
        let mut startup = StartupEnvironment::from_reload_marker(true);
        assert!(startup.take_resume_agents_after_reload());
        assert!(!startup.take_resume_agents_after_reload());
    }

    #[test]
    fn ambient_infisical_token_names_are_narrowly_blocked() {
        assert!(is_ambient_infisical_token("INFISICAL_TOKEN"));
        assert!(is_ambient_infisical_token("INFISICAL_SERVICE_TOKEN"));

        // Non-token routing and machine-identity inputs remain available so a
        // consumer can explicitly mint its own token.
        assert!(!is_ambient_infisical_token("INFISICAL_URL"));
        assert!(!is_ambient_infisical_token("INFISICAL_PROJECT_ID"));
        assert!(!is_ambient_infisical_token("INFISICAL_CLIENT_ID"));
        assert!(!is_ambient_infisical_token("INFISICAL_CLIENT_SECRET"));
    }

    #[test]
    fn startup_wipe_clears_every_duplicate_token_value() {
        let mut first = b"INFISICAL_TOKEN=first\0".to_vec();
        let mut second = b"INFISICAL_TOKEN=second\0".to_vec();
        let mut safe = b"INFISICAL_URL=https://secrets.example\0".to_vec();
        let mut entries = vec![
            first.as_mut_ptr().cast::<libc::c_char>(),
            second.as_mut_ptr().cast::<libc::c_char>(),
            safe.as_mut_ptr().cast::<libc::c_char>(),
            std::ptr::null_mut(),
        ];

        // SAFETY: the test builds a writable NULL-terminated environment vector
        // whose backing byte buffers outlive this call.
        unsafe { wipe_environment_entries(entries.as_mut_ptr(), "INFISICAL_TOKEN") };

        let prefix_len = b"INFISICAL_TOKEN=".len();
        assert!(first[prefix_len..first.len() - 1]
            .iter()
            .all(|byte| *byte == 0));
        assert!(second[prefix_len..second.len() - 1]
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(safe, b"INFISICAL_URL=https://secrets.example\0");
    }

    #[test]
    fn child_command_removes_sensitive_tokens_and_internal_reload_marker() {
        let mut command = Command::new("true");
        command.env("INFISICAL_TOKEN", "synthetic-parent-token");
        command.env("INFISICAL_SERVICE_TOKEN", "synthetic-service-token");

        prepare_child_command(
            &mut command,
            &[
                ("INFISICAL_TOKEN".into(), "synthetic-override".into()),
                ("INFISICAL_URL".into(), "https://secrets.example".into()),
                (RELOAD_RESUME_ENV.into(), "must-not-reach-pane".into()),
                ("TAAROF_SAFE_TEST".into(), "kept".into()),
            ],
        );

        let env: HashMap<OsString, Option<OsString>> = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsString::from)))
            .collect();
        assert_eq!(env.get(&OsString::from("INFISICAL_TOKEN")), Some(&None));
        assert_eq!(
            env.get(&OsString::from("TAAROF_RELOAD_RESUME_AGENTS")),
            Some(&None)
        );
        assert_eq!(
            env.get(&OsString::from("INFISICAL_SERVICE_TOKEN")),
            Some(&None)
        );
        assert_eq!(
            env.get(&OsString::from("INFISICAL_URL")),
            Some(&Some(OsString::from("https://secrets.example")))
        );
        assert_eq!(
            env.get(&OsString::from("TAAROF_SAFE_TEST")),
            Some(&Some(OsString::from("kept")))
        );
    }
}
