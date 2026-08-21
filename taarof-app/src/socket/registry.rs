//! Socket registry: JSON file that records the active socket path and PID.

use super::*;
use std::sync::{Mutex, OnceLock};

/// The registry is the discovery file external tools read before they can talk
/// to anything. Carrying identity here means `taarof --version` and Doctor can
/// answer "what is actually running" without a socket round-trip — and see the
/// same answer the app's own UI shows.
///
/// `identity` is `#[serde(default)]` so a registry written by an older app (or
/// read by an older CLI) still parses; a missing block reads as unknown, never
/// as current.
#[derive(Serialize, Deserialize)]
pub(super) struct SocketRegistry {
    pub(super) pid: u32,
    pub(super) socket_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) identity: Option<serde_json::Value>,
}

pub(super) fn registry_file_name() -> String {
    match instance::session_storage_key() {
        Some(session) => format!("taarof-current-{session}.json"),
        None => "taarof-current.json".to_string(),
    }
}

pub(super) fn registry_path_in(dir: &Path) -> PathBuf {
    dir.join(registry_file_name())
}

pub(super) fn socket_registry_path_for_socket(socket_path: &Path) -> Option<PathBuf> {
    Some(registry_path_in(socket_path.parent()?))
}

/// Remembered at bind time so the update-watch worker can rewrite the registry
/// from a background thread without re-deriving session-scoped paths there.
fn active_registry_path() -> &'static Mutex<Option<PathBuf>> {
    static ACTIVE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(None))
}

fn write_registry_file(path: &Path, registry: &SocketRegistry) {
    match serde_json::to_string_pretty(registry) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                eprintln!(
                    "taarof: could not write socket registry {}: {e}",
                    path.display()
                );
            }
        }
        Err(e) => eprintln!("taarof: could not serialize socket registry: {e}"),
    }
}

pub(super) fn write_socket_registry(socket_path: &Path) {
    let Some(path) = socket_registry_path_for_socket(socket_path) else {
        return;
    };
    // Startup writes only the compile-time half of identity. Hashing and source
    // discovery are deliberately absent here: they would block the launch→first
    // usable tab window. `refresh_registry_identity` fills in the rest from the
    // update-watch worker, and until it does the file honestly says unknown.
    let registry = SocketRegistry {
        pid: std::process::id(),
        socket_path: socket_path.display().to_string(),
        identity: serde_json::to_value(crate::runtime_identity::cached()).ok(),
    };
    *active_registry_path()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path.clone());
    write_registry_file(&path, &registry);
}

/// Rewrite the registry's identity block once a probe completes.
///
/// Called from the update-watch blocking worker, never from a GTK callback.
/// Failure is silent-but-not-lying: if the file cannot be rewritten, the
/// previous (more conservative) identity stays on disk.
pub(crate) fn refresh_registry_identity(identity: &crate::runtime_identity::RuntimeIdentity) {
    let path = {
        let guard = active_registry_path()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(path) => path.clone(),
            None => return,
        }
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut registry) = serde_json::from_str::<SocketRegistry>(&contents) else {
        return;
    };
    // Another instance may have taken over the registry between the probe
    // starting and finishing; do not overwrite its identity with ours.
    if registry.pid != std::process::id() {
        return;
    }
    registry.identity = serde_json::to_value(identity).ok();
    write_registry_file(&path, &registry);
}

pub(super) fn cleanup_socket_registry(socket_path: &Path) {
    let Some(path) = socket_registry_path_for_socket(socket_path) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(registry) = serde_json::from_str::<SocketRegistry>(&content) else {
        return;
    };
    if registry.socket_path == socket_path.display().to_string() {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn cleanup_stale_socket_registry_in(dir: &Path) {
    cleanup_stale_socket_registry_file(&registry_path_in(dir));
}

pub(super) fn cleanup_stale_socket_registry_file(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let should_remove = match serde_json::from_str::<SocketRegistry>(&content) {
        Ok(registry) => !process_exists(registry.pid),
        Err(_) => true,
    };
    if should_remove {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn process_exists(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
