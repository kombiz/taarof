//! Local mise binary discovery and task discovery helpers.

use super::*;

/// Resolve the mise binary path, honoring an explicit override first.
pub(super) fn normalize_mise_binary_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct FindMiseTestProbe {
    pub(super) delay: Duration,
    pub(super) result: Option<PathBuf>,
    pub(super) calls: usize,
    pub(super) thread_id: std::thread::ThreadId,
}

#[cfg(test)]
pub(super) fn find_mise_test_probe() -> &'static Mutex<Option<FindMiseTestProbe>> {
    static PROBE: OnceLock<Mutex<Option<FindMiseTestProbe>>> = OnceLock::new();
    PROBE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(super) fn run_find_mise_test_probe() -> Option<Option<PathBuf>> {
    let (delay, result) = {
        let thread_id = std::thread::current().id();
        let mut probe = find_mise_test_probe()
            .lock()
            .expect("find_mise test probe lock should not be poisoned");
        let probe = probe.as_mut()?;
        if probe.thread_id != thread_id {
            return None;
        }
        probe.calls += 1;
        (probe.delay, probe.result.clone())
    };
    std::thread::sleep(delay);
    Some(result)
}

#[cfg(test)]
pub(crate) fn install_find_mise_test_probe(result: Option<PathBuf>, delay: Duration) {
    find_mise_test_probe()
        .lock()
        .expect("find_mise test probe lock should not be poisoned")
        .replace(FindMiseTestProbe {
            delay,
            result,
            calls: 0,
            thread_id: std::thread::current().id(),
        });
}

#[cfg(test)]
pub(crate) fn clear_find_mise_test_probe() {
    find_mise_test_probe()
        .lock()
        .expect("find_mise test probe lock should not be poisoned")
        .take();
}

#[cfg(test)]
pub(crate) fn find_mise_test_probe_call_count() -> usize {
    find_mise_test_probe()
        .lock()
        .expect("find_mise test probe lock should not be poisoned")
        .as_ref()
        .map(|probe| probe.calls)
        .unwrap_or(0)
}

pub(super) fn find_mise() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(result) = run_find_mise_test_probe() {
        return result;
    }

    if let Some(path) = std::env::var_os("MISE_BIN").filter(|path| !path.is_empty()) {
        return Some(normalize_mise_binary_path(Path::new(&path)));
    }

    // Fall back to PATH via which-style lookup.
    if let Ok(output) = Command::new("which").arg("mise").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(normalize_mise_binary_path(Path::new(&path)));
            }
        }
    }

    // Fallback: check common install locations
    for candidate in ["/usr/bin/mise", "/usr/local/bin/mise"] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(normalize_mise_binary_path(&p));
        }
    }

    // Check ~/.local/bin/mise (mise self-install location)
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".local/bin/mise");
        if p.exists() {
            return Some(normalize_mise_binary_path(&p));
        }
    }

    None
}

#[cfg(test)]
pub(crate) fn local_discovery_target(cwd: &str) -> DiscoveryTarget {
    DiscoveryTarget::Local {
        cwd: cwd.to_string(),
        binary_path: find_mise(),
    }
}

/// Populate `state.discovery_binary_paths` for `tab_id` so that subsequent
/// task execution reuses the same mise binary that discovery resolved.
///
/// Discovery and execution can otherwise observe different `PATH` snapshots
/// (e.g. discovery from a GUI launcher's PATH, execution from a login
/// shell's), causing tasks that show up in the palette to fail at run time.
/// To keep behavior consistent, this function is the single
/// writer of the cache and prefers, in order:
///   1. an explicit `binary_path` carried on the `Local` target,
///   2. a previously cached entry for this tab (no-op),
///   3. a fresh `find_mise()` resolution.
///
/// `Remote` targets clear the local cache entry — the remote binary path is
/// resolved on the remote host, not from this process's `PATH`.
pub(crate) fn update_discovery_binary_cache(
    state: &mut crate::AppState,
    tab_id: u32,
    target: &DiscoveryTarget,
) {
    match target {
        DiscoveryTarget::Local { binary_path, .. } => {
            if let Some(path) = binary_path.clone() {
                state.discovery_binary_paths.insert(tab_id, path);
            } else if let std::collections::hash_map::Entry::Vacant(entry) =
                state.discovery_binary_paths.entry(tab_id)
            {
                if let Some(resolved) = find_mise() {
                    entry.insert(resolved);
                }
            }
        }
        DiscoveryTarget::Remote { .. } => {
            state.discovery_binary_paths.remove(&tab_id);
        }
    }
}

pub(super) fn record_missing_mise_binary(operation: &str, details: serde_json::Value) {
    crate::diagnostics::record_command_failure(
        "mise",
        operation,
        MISSING_MISE_BINARY_MESSAGE,
        Some(details),
    );
}

pub(super) fn resolve_local_mise_binary(binary_path: &Option<PathBuf>) -> Option<PathBuf> {
    binary_path
        .as_ref()
        .cloned()
        .or_else(find_mise)
        .map(|path| normalize_mise_binary_path(&path))
}

pub(super) fn require_local_mise_binary(
    binary_path: &Option<PathBuf>,
    operation: &str,
    details: serde_json::Value,
) -> Option<PathBuf> {
    let Some(binary_path) = resolve_local_mise_binary(binary_path) else {
        record_missing_mise_binary(operation, details);
        return None;
    };

    Some(binary_path)
}

/// Run `mise tasks ls --json`, retrying briefly on `ExecutableFileBusy`.
///
/// On Linux a freshly written executable can transiently fail `execve` with
/// `ETXTBSY` ("Text file busy") while another thread in the process holds a
/// writable descriptor to it across a concurrent `fork`. This is a known
/// multithreaded write-then-exec race that surfaces under highly parallel test
/// runs (many fixtures being written and executed at once) but can also occur in
/// production right after a mise self-install. A short bounded retry lets the
/// offending descriptor close without changing observable behavior.
fn run_mise_tasks_ls(mise_bin: &Path, cwd: &str) -> std::io::Result<std::process::Output> {
    const MAX_ATTEMPTS: u32 = 20;
    let mut attempt = 0;
    loop {
        match Command::new(mise_bin)
            .args(["tasks", "ls", "--json"])
            .current_dir(cwd)
            .output()
        {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < MAX_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

pub(super) fn discover_tasks_with_binary(cwd: &str, mise_bin: &Path) -> Vec<MiseTask> {
    let output = run_mise_tasks_ls(mise_bin, cwd);

    match output {
        Ok(out) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                crate::diagnostics::record_command_failure(
                    "mise",
                    "discover",
                    format!("mise failed in {cwd}"),
                    Some(serde_json::json!({
                        "cwd": cwd,
                        "mise_bin": mise_bin,
                        "status": out.status.to_string(),
                        "stderr": stderr.trim(),
                    })),
                );
                return Vec::new();
            }
            let tasks: Vec<MiseTask> = serde_json::from_slice(&out.stdout).unwrap_or_default();
            eprintln!("taarof: mise found {} tasks in {cwd}", tasks.len());
            tasks.into_iter().filter(|t| !t.hide).collect()
        }
        Err(e) => {
            crate::diagnostics::record_command_failure(
                "mise",
                "discover",
                format!("mise failed to run in {cwd}"),
                Some(serde_json::json!({
                    "cwd": cwd,
                    "mise_bin": mise_bin,
                    "error": e.to_string(),
                })),
            );
            Vec::new()
        }
    }
}

pub(super) const MISE_CONFIG_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    "mise.dev.toml",
    "mise.prod.toml",
];
pub(super) const MISE_TASK_DIR: &str = ".mise/tasks";

pub(super) fn has_mise_config(dir: &std::path::Path) -> bool {
    nearest_mise_config_dir(dir).is_some()
}

pub(super) fn nearest_mise_config_dir(dir: &std::path::Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }

    let mut current = dir.to_path_buf();
    loop {
        if MISE_CONFIG_FILES
            .iter()
            .any(|name| current.join(name).is_file())
            || current.join(MISE_TASK_DIR).is_dir()
        {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(super) fn first_existing_dir(candidates: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    candidates.iter().find(|path| path.is_dir()).cloned()
}

pub(super) fn best_discovery_dir(
    candidates: Vec<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    candidates
        .iter()
        .find(|path| has_mise_config(path))
        .cloned()
        .or_else(|| first_existing_dir(&candidates))
}

pub(super) fn task_source_dir(task: &MiseTask) -> Option<&Path> {
    let source = Path::new(task.source.as_deref()?);
    source.parent().or(Some(source))
}

pub(super) fn classify_task_scope(task: &MiseTask, discovery_root: &Path) -> MiseTaskScope {
    if let Some(source_dir) = task_source_dir(task) {
        if source_dir.starts_with(discovery_root) {
            return MiseTaskScope::ProjectRoot;
        }

        if discovery_root.starts_with(source_dir) {
            return MiseTaskScope::ParentDir;
        }
    }

    if task.global {
        MiseTaskScope::Global
    } else {
        // Without a usable source path, keep non-global tasks rather than dropping them.
        MiseTaskScope::ProjectRoot
    }
}

pub(super) fn filter_tasks_for_target(
    tasks: Vec<MiseTask>,
    target: &DiscoveryTarget,
) -> Vec<MiseTask> {
    let include_global = crate::config::mise_config().include_global;
    if include_global {
        return tasks;
    }

    let discovery_root = match target {
        DiscoveryTarget::Local { cwd, .. } | DiscoveryTarget::Remote { cwd, .. } => Path::new(cwd),
    };

    tasks
        .into_iter()
        .filter(|task| classify_task_scope(task, discovery_root) != MiseTaskScope::Global)
        .collect()
}
