//! Cached inspection of the running and installed taarof executables.
//!
//! All filesystem and hashing work runs on a blocking worker. Snapshot readers
//! only clone the last completed value from the process-global cache.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UPDATE_CHECK_TTL: Duration = Duration::from_secs(60);
// Poll more frequently than the TTL. Freshness begins when the asynchronous
// worker completes, not when the timer was installed; equality here lets a
// slow initial worker make the first post-completion tick arrive just before
// expiry, delaying the next inspection for another full timer period.
const UPDATE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DELETED_SUFFIX: &str = " (deleted)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Current,
    UpdatePending,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateReason {
    RunningExecutableDeleted,
    ContentMismatch,
    BuildIdMismatch,
    InstalledBinaryMissing,
    InstalledUnreadable,
    RunningUnreadable,
    HashFailed,
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BinaryIdentity {
    pub path: Option<String>,
    pub resolved_path: Option<String>,
    pub symlink: bool,
    pub deleted: bool,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub build_id: Option<String>,
    pub readable: bool,
    pub error: Option<String>,
}

impl BinaryIdentity {
    pub(crate) fn unknown(error: &str) -> Self {
        Self {
            path: None,
            resolved_path: None,
            symlink: false,
            deleted: false,
            size_bytes: None,
            sha256: None,
            build_id: None,
            readable: false,
            error: Some(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    pub schema: &'static str,
    pub state: UpdateState,
    pub reason: Option<UpdateReason>,
    pub running: BinaryIdentity,
    pub installed: BinaryIdentity,
    pub content_matches: Option<bool>,
    pub checked_at_unix_ms: Option<u64>,
    pub restart_required: bool,
    pub restart_policy: &'static str,
}

impl UpdateStatus {
    pub(crate) fn new(
        state: UpdateState,
        reason: Option<UpdateReason>,
        running: BinaryIdentity,
        installed: BinaryIdentity,
        checked_at_unix_ms: Option<u64>,
    ) -> Self {
        let content_matches = match (&running.sha256, &installed.sha256) {
            (Some(running), Some(installed)) => Some(running == installed),
            _ => None,
        };
        Self {
            schema: "taarof.update.v1",
            state,
            reason,
            running,
            installed,
            content_matches,
            checked_at_unix_ms,
            restart_required: state == UpdateState::UpdatePending,
            restart_policy: "operator",
        }
    }

    fn not_checked() -> Self {
        Self::new(
            UpdateState::Unknown,
            None,
            BinaryIdentity::unknown("not_checked"),
            BinaryIdentity::unknown("not_checked"),
            None,
        )
    }
}

#[derive(Debug, Clone)]
enum UpdateWatchCache {
    Missing,
    Pending,
    Ready {
        status: Box<UpdateStatus>,
        completed_at: Instant,
    },
}

fn cache() -> &'static Mutex<UpdateWatchCache> {
    static CACHE: OnceLock<Mutex<UpdateWatchCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(UpdateWatchCache::Missing))
}

type StatusListener = Box<dyn Fn(&UpdateStatus)>;

thread_local! {
    static STATUS_LISTENERS: RefCell<Vec<StatusListener>> = RefCell::new(Vec::new());
}

pub fn connect_status_changed<F>(listener: F)
where
    F: Fn(&UpdateStatus) + 'static,
{
    STATUS_LISTENERS.with(|listeners| listeners.borrow_mut().push(Box::new(listener)));
}

fn notify_status_changed(status: &UpdateStatus) {
    STATUS_LISTENERS.with(|listeners| {
        for listener in listeners.borrow().iter() {
            listener(status);
        }
    });
}

pub fn cached_status() -> UpdateStatus {
    match &*cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        UpdateWatchCache::Ready { status, .. } => status.as_ref().clone(),
        UpdateWatchCache::Missing | UpdateWatchCache::Pending => UpdateStatus::not_checked(),
    }
}

fn prepare_refresh(now: Instant, ttl: Duration) -> bool {
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    prepare_cache_refresh(&mut cache, now, ttl)
}

fn prepare_cache_refresh(cache: &mut UpdateWatchCache, now: Instant, ttl: Duration) -> bool {
    match &*cache {
        UpdateWatchCache::Pending => false,
        UpdateWatchCache::Ready { completed_at, .. }
            if now.saturating_duration_since(*completed_at) < ttl =>
        {
            false
        }
        UpdateWatchCache::Missing | UpdateWatchCache::Ready { .. } => {
            *cache = UpdateWatchCache::Pending;
            true
        }
    }
}

fn complete_refresh(status: UpdateStatus, completed_at: Instant) {
    *cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = UpdateWatchCache::Ready {
        status: Box::new(status),
        completed_at,
    };
}

/// Everything expensive in one place, and all of it off the GTK thread:
/// hashing both binaries, running `git` against the checkout, reading the
/// install manifest, and rewriting the runtime registry so external tools see
/// the same identity the app does.
fn compute_refresh() -> (UpdateStatus, crate::runtime_identity::RuntimeIdentity) {
    let status = compute_status();
    let identity = crate::runtime_identity::compute(&status);
    crate::socket::refresh_registry_identity(&identity);
    (status, identity)
}

pub fn spawn_refresh() {
    if !prepare_refresh(Instant::now(), UPDATE_CHECK_TTL) {
        return;
    }

    glib::MainContext::default().spawn(async move {
        let (status, identity) = match gio::spawn_blocking(compute_refresh).await {
            Ok(result) => result,
            Err(_) => {
                let status = UpdateStatus::new(
                    UpdateState::Unknown,
                    Some(UpdateReason::HashFailed),
                    BinaryIdentity::unknown("worker_failed"),
                    BinaryIdentity::unknown("worker_failed"),
                    Some(unix_time_ms()),
                );
                let identity = crate::runtime_identity::RuntimeIdentity::not_checked();
                (status, identity)
            }
        };
        complete_refresh(status.clone(), Instant::now());
        // Store identity before notifying: listeners read the cache, and a
        // listener that saw a stale identity would render the very mismatch
        // this module exists to eliminate.
        crate::runtime_identity::store(identity);
        notify_status_changed(&status);
    });
}

/// Polling more frequently than the TTL bounds refresh latency after an async
/// completion without relaxing the cache's single-flight and freshness rules.
pub fn poll_interval() -> Duration {
    UPDATE_POLL_INTERVAL
}

pub fn dialog_actions() -> &'static [&'static str] {
    &["reload", "copy-details", "close"]
}

fn reload_handoff() -> &'static Mutex<Option<UnixStream>> {
    static HANDOFF: OnceLock<Mutex<Option<UnixStream>>> = OnceLock::new();
    HANDOFF.get_or_init(|| Mutex::new(None))
}

fn spawn_reload_helper(
    installed: &Path,
    args: &[std::ffi::OsString],
) -> io::Result<(Child, UnixStream)> {
    let (lifetime, helper_wait) = UnixStream::pair()?;
    let helper_wait = OwnedFd::from(helper_wait);
    // Deliberately outlives us: the helper blocks until taarof exits, then execs
    // the installed binary. taarof is on its way down, so it cannot reap this and
    // there is nothing left to accumulate zombies in.
    #[allow(clippy::disallowed_methods)]
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg("while IFS= read -r _; do :; done; exec \"$@\"")
        .arg("taarof-reload")
        .arg(installed)
        .args(args)
        .env(crate::child_env::RELOAD_RESUME_ENV, "1")
        .stdin(Stdio::from(helper_wait))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok((child, lifetime))
}

/// Start a detached helper that waits for this process to exit, then execs the
/// installed binary with the original arguments. The caller closes the window
/// only after this succeeds, allowing the ordinary close path to persist the
/// pane tree and live agent session identities first.
pub fn spawn_installed_reload(status: &UpdateStatus) -> Result<(), String> {
    if status.state != UpdateState::UpdatePending {
        return Err("no installed update is pending".to_string());
    }
    let installed = status
        .installed
        .path
        .as_deref()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "installed binary path is unavailable".to_string())?;
    if !status.installed.readable {
        return Err("installed binary is not readable".to_string());
    }

    let mut handoff = reload_handoff()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if handoff.is_some() {
        return Err("an installed-update reload is already in progress".to_string());
    }

    let args: Vec<_> = env::args_os().skip(1).collect();
    let (_helper, lifetime) = spawn_reload_helper(installed, &args)
        .map_err(|error| format!("could not start reload helper: {error}"))?;
    *handoff = Some(lifetime);
    Ok(())
}

pub fn evaluate(
    platform_linux: bool,
    running: &BinaryIdentity,
    installed: &BinaryIdentity,
) -> (UpdateState, Option<UpdateReason>) {
    if !platform_linux {
        return (
            UpdateState::Unknown,
            Some(UpdateReason::UnsupportedPlatform),
        );
    }
    if !running.readable {
        return (UpdateState::Unknown, Some(UpdateReason::RunningUnreadable));
    }
    if installed.resolved_path.is_none() {
        return (
            UpdateState::Unknown,
            Some(UpdateReason::InstalledBinaryMissing),
        );
    }
    if !installed.readable {
        return (
            UpdateState::Unknown,
            Some(UpdateReason::InstalledUnreadable),
        );
    }
    if running.sha256.is_none() || installed.sha256.is_none() {
        return (UpdateState::Unknown, Some(UpdateReason::HashFailed));
    }
    if running.deleted {
        return (
            UpdateState::UpdatePending,
            Some(UpdateReason::RunningExecutableDeleted),
        );
    }
    if let (Some(running_build_id), Some(installed_build_id)) =
        (&running.build_id, &installed.build_id)
    {
        if running_build_id != installed_build_id {
            return (
                UpdateState::UpdatePending,
                Some(UpdateReason::BuildIdMismatch),
            );
        }
    }
    if running.sha256 != installed.sha256 {
        return (
            UpdateState::UpdatePending,
            Some(UpdateReason::ContentMismatch),
        );
    }
    (UpdateState::Current, None)
}

fn compute_status() -> UpdateStatus {
    if !cfg!(target_os = "linux") {
        let running = BinaryIdentity::unknown("unsupported_platform");
        let installed = BinaryIdentity::unknown("unsupported_platform");
        let (state, reason) = evaluate(false, &running, &installed);
        return UpdateStatus::new(state, reason, running, installed, Some(unix_time_ms()));
    }

    let running = inspect_running();
    let installed = inspect_installed(&installed_binary_path());
    let (state, reason) = evaluate(true, &running, &installed);
    UpdateStatus::new(state, reason, running, installed, Some(unix_time_ms()))
}

fn installed_binary_path() -> PathBuf {
    if let Some(path) = crate::config::update_config().installed_binary_path {
        return path;
    }
    if let Some(path) = env::var_os("TAAROF_INSTALLED_BINARY")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return path;
    }

    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/taarof-app"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/taarof-app"));
    candidates.push(PathBuf::from("/usr/bin/taarof-app"));
    candidates
        .iter()
        .find(|path| fs::symlink_metadata(path).is_ok())
        .cloned()
        .or_else(|| candidates.into_iter().next())
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/taarof-app"))
}

fn inspect_running() -> BinaryIdentity {
    let proc_exe = Path::new("/proc/self/exe");
    let link = match fs::read_link(proc_exe) {
        Ok(link) => link,
        Err(_) => return BinaryIdentity::unknown("read_link_failed"),
    };
    let (reported_path, deleted) = parse_deleted_notation(&link.to_string_lossy());
    inspect_file(Some(reported_path), proc_exe, false, deleted, hash_file)
}

fn inspect_installed(path: &Path) -> BinaryIdentity {
    let reported = path.to_string_lossy().into_owned();
    let symlink = fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    inspect_file(Some(reported), path, symlink, false, hash_file)
}

fn inspect_file<F>(
    reported_path: Option<String>,
    read_path: &Path,
    symlink: bool,
    deleted: bool,
    hasher: F,
) -> BinaryIdentity
where
    F: FnOnce(&Path) -> io::Result<String>,
{
    let resolved_path = fs::canonicalize(read_path)
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let metadata = match fs::metadata(read_path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => {
            return BinaryIdentity {
                path: reported_path,
                resolved_path,
                symlink,
                deleted,
                size_bytes: None,
                sha256: None,
                build_id: None,
                readable: false,
                error: Some("missing".to_string()),
            };
        }
    };

    if File::open(read_path).is_err() {
        return BinaryIdentity {
            path: reported_path,
            resolved_path,
            symlink,
            deleted,
            size_bytes: Some(metadata.len()),
            sha256: None,
            build_id: None,
            readable: false,
            error: Some("unreadable".to_string()),
        };
    }

    match hasher(read_path) {
        Ok(sha256) => BinaryIdentity {
            path: reported_path,
            resolved_path,
            symlink,
            deleted,
            size_bytes: Some(metadata.len()),
            sha256: Some(sha256),
            build_id: read_build_id(read_path),
            readable: true,
            error: None,
        },
        Err(_) => BinaryIdentity {
            path: reported_path,
            resolved_path,
            symlink,
            deleted,
            size_bytes: Some(metadata.len()),
            sha256: None,
            build_id: None,
            readable: true,
            error: Some("hash_failed".to_string()),
        },
    }
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Best-effort GNU build-id extraction. A malformed or unfamiliar ELF image
/// simply has no build id; SHA-256 remains the primary identity.
fn read_build_id(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.get(..4)? != b"\x7fELF" {
        return None;
    }
    let little_endian = match bytes.get(5)? {
        1 => true,
        2 => false,
        _ => return None,
    };
    let read_u32 = |slice: &[u8]| -> Option<u32> {
        let bytes: [u8; 4] = slice.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };

    for offset in (0..bytes.len().saturating_sub(16)).step_by(4) {
        let namesz = read_u32(bytes.get(offset..offset + 4)?)? as usize;
        let descsz = read_u32(bytes.get(offset + 4..offset + 8)?)? as usize;
        let note_type = read_u32(bytes.get(offset + 8..offset + 12)?)?;
        if namesz != 4 || note_type != 3 || descsz == 0 || descsz > 128 {
            continue;
        }
        if bytes.get(offset + 12..offset + 16)? != b"GNU\0" {
            continue;
        }
        let desc_start = offset + 16;
        let desc = bytes.get(desc_start..desc_start.checked_add(descsz)?)?;
        return Some(desc.iter().map(|byte| format!("{byte:02x}")).collect());
    }
    None
}

fn parse_deleted_notation(value: &str) -> (String, bool) {
    match value.strip_suffix(DELETED_SUFFIX) {
        Some(path) => (path.to_string(), true),
        None => (value.to_string(), false),
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
pub(crate) fn set_cached_status_for_test(status: UpdateStatus) {
    complete_refresh(status, Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "taarof-update-watch-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn identity(path: &Path) -> BinaryIdentity {
        inspect_file(
            Some(path.to_string_lossy().into_owned()),
            path,
            false,
            false,
            hash_file,
        )
    }

    #[test]
    fn matching_and_replaced_binaries_are_distinguished() {
        let dir = temp_dir("content");
        let running_path = dir.join("running");
        let installed_path = dir.join("installed");
        fs::write(&running_path, b"same").unwrap();
        fs::write(&installed_path, b"same").unwrap();
        let running = identity(&running_path);
        let installed = identity(&installed_path);
        assert_eq!(
            evaluate(true, &running, &installed),
            (UpdateState::Current, None)
        );

        fs::write(&installed_path, b"replacement").unwrap();
        let installed = identity(&installed_path);
        assert_eq!(
            evaluate(true, &running, &installed),
            (
                UpdateState::UpdatePending,
                Some(UpdateReason::ContentMismatch)
            )
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stable_build_identity_mismatch_precedes_content_mismatch() {
        let dir = temp_dir("build-id");
        let running_path = dir.join("running");
        let installed_path = dir.join("installed");
        fs::write(&running_path, b"running").unwrap();
        fs::write(&installed_path, b"installed").unwrap();
        let mut running = identity(&running_path);
        let mut installed = identity(&installed_path);
        running.build_id = Some("build-a".to_string());
        installed.build_id = Some("build-b".to_string());

        assert_eq!(
            evaluate(true, &running, &installed),
            (
                UpdateState::UpdatePending,
                Some(UpdateReason::BuildIdMismatch)
            )
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn deleted_notation_wins_even_when_content_matches() {
        let dir = temp_dir("deleted");
        let path = dir.join("binary");
        fs::write(&path, b"same").unwrap();
        let (reported, deleted) = parse_deleted_notation("/tmp/taarof-app (deleted)");
        assert_eq!(reported, "/tmp/taarof-app");
        assert!(deleted);
        let running = inspect_file(Some(reported), &path, false, deleted, hash_file);
        let installed = identity(&path);
        let (state, reason) = evaluate(true, &running, &installed);
        let status = UpdateStatus::new(state, reason, running, installed, Some(1));
        assert_eq!(status.state, UpdateState::UpdatePending);
        assert_eq!(status.reason, Some(UpdateReason::RunningExecutableDeleted));
        assert_eq!(status.content_matches, Some(true));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_install_resolves_and_dangling_install_is_missing() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink");
        let running_path = dir.join("running");
        let target = dir.join("target");
        let link = dir.join("installed");
        fs::write(&running_path, b"same").unwrap();
        fs::write(&target, b"same").unwrap();
        symlink(&target, &link).unwrap();
        let running = identity(&running_path);
        let installed = inspect_installed(&link);
        assert!(installed.symlink);
        assert_eq!(
            installed.resolved_path,
            Some(target.to_string_lossy().into_owned())
        );
        assert_eq!(
            evaluate(true, &running, &installed),
            (UpdateState::Current, None)
        );

        fs::remove_file(&target).unwrap();
        let installed = inspect_installed(&link);
        assert_eq!(
            evaluate(true, &running, &installed),
            (
                UpdateState::Unknown,
                Some(UpdateReason::InstalledBinaryMissing)
            )
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_unreadable_and_hash_failure_are_unknown() {
        let dir = temp_dir("failure");
        let running_path = dir.join("running");
        let installed_path = dir.join("installed");
        fs::write(&running_path, b"running").unwrap();
        let running = identity(&running_path);
        let missing = inspect_installed(&installed_path);
        assert_eq!(
            evaluate(true, &running, &missing).1,
            Some(UpdateReason::InstalledBinaryMissing)
        );

        fs::write(&installed_path, b"installed").unwrap();
        let failed_hash = inspect_file(
            Some(installed_path.to_string_lossy().into_owned()),
            &installed_path,
            false,
            false,
            |_| Err(io::Error::other("injected")),
        );
        assert!(failed_hash.readable);
        assert_eq!(
            evaluate(true, &running, &failed_hash).1,
            Some(UpdateReason::HashFailed)
        );

        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&installed_path, fs::Permissions::from_mode(0o0)).unwrap();
            let unreadable = inspect_installed(&installed_path);
            assert_eq!(
                evaluate(true, &running, &unreadable).1,
                Some(UpdateReason::InstalledUnreadable)
            );
            fs::set_permissions(&installed_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_platform_is_unknown() {
        let identity = BinaryIdentity::unknown("test");
        assert_eq!(
            evaluate(false, &identity, &identity),
            (
                UpdateState::Unknown,
                Some(UpdateReason::UnsupportedPlatform)
            )
        );
    }

    #[test]
    fn cache_deduplicates_pending_reuses_fresh_and_expires() {
        let mut cache = UpdateWatchCache::Missing;
        let now = Instant::now();
        let probes = AtomicUsize::new(0);
        if prepare_cache_refresh(&mut cache, now, UPDATE_CHECK_TTL) {
            probes.fetch_add(1, Ordering::Relaxed);
        }
        assert!(!prepare_cache_refresh(&mut cache, now, UPDATE_CHECK_TTL));
        assert_eq!(probes.load(Ordering::Relaxed), 1);

        let status = UpdateStatus::not_checked();
        cache = UpdateWatchCache::Ready {
            status: Box::new(status),
            completed_at: now,
        };
        assert!(!prepare_cache_refresh(
            &mut cache,
            now + Duration::from_secs(1),
            UPDATE_CHECK_TTL
        ));
        assert!(prepare_cache_refresh(
            &mut cache,
            now + UPDATE_CHECK_TTL,
            UPDATE_CHECK_TTL
        ));
        probes.fetch_add(1, Ordering::Relaxed);
        assert_eq!(probes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn timer_epoch_before_initial_completion_refreshes_within_one_ttl() {
        let mut cache = UpdateWatchCache::Ready {
            status: Box::new(UpdateStatus::not_checked()),
            // Model a timer installed at epoch 0 whose initial asynchronous
            // check did not finish until 10 seconds later. With a 60-second
            // timer, the tick at 60 seconds sees a fresh cache and the next
            // attempt would not occur until 120 seconds.
            completed_at: Instant::now(),
        };
        let completed_at = match &cache {
            UpdateWatchCache::Ready { completed_at, .. } => *completed_at,
            _ => unreachable!(),
        };
        assert!(poll_interval() < UPDATE_CHECK_TTL);
        let timer_epoch = completed_at - Duration::from_secs(10);
        assert!(!prepare_cache_refresh(
            &mut cache,
            timer_epoch + Duration::from_secs(60),
            UPDATE_CHECK_TTL
        ));
        assert!(prepare_cache_refresh(
            &mut cache,
            timer_epoch + Duration::from_secs(120),
            UPDATE_CHECK_TTL
        ));

        // The production cadence has a tick at 90 seconds, so it refreshes
        // 80 seconds after completion rather than waiting until 120 seconds.
        let mut cache = UpdateWatchCache::Ready {
            status: Box::new(UpdateStatus::not_checked()),
            completed_at,
        };
        assert!(prepare_cache_refresh(
            &mut cache,
            timer_epoch + Duration::from_secs(90),
            UPDATE_CHECK_TTL
        ));
    }

    #[test]
    fn operator_reload_contract_is_machine_checkable() {
        let status = UpdateStatus::new(
            UpdateState::UpdatePending,
            Some(UpdateReason::ContentMismatch),
            BinaryIdentity::unknown("test"),
            BinaryIdentity::unknown("test"),
            Some(1),
        );
        assert_eq!(dialog_actions(), &["reload", "copy-details", "close"]);
        assert_eq!(status.restart_policy, "operator");
        assert!(status.restart_required);
    }

    #[test]
    fn reload_helper_waits_for_original_process_lifetime_eof() {
        let dir = temp_dir("reload-eof");
        let marker = dir.join("reloaded");
        let args = vec![
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from(format!(
                "test \"$TAAROF_RELOAD_RESUME_AGENTS\" = 1 && printf ready > {}",
                marker.display()
            )),
        ];
        let (mut helper, lifetime) =
            spawn_reload_helper(Path::new("/bin/sh"), &args).expect("helper should start");

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "helper must wait while the app endpoint lives"
        );

        drop(lifetime);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if helper.try_wait().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "helper did not observe EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), "ready");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn installed_reload_rejects_non_pending_status() {
        let status = UpdateStatus::new(
            UpdateState::Current,
            None,
            BinaryIdentity::unknown("test"),
            BinaryIdentity::unknown("test"),
            Some(1),
        );
        assert_eq!(
            spawn_installed_reload(&status),
            Err("no installed update is pending".to_string())
        );
    }
}
