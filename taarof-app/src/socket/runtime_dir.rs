//! Runtime-directory discovery and validation for the Unix socket server.

use super::*;
use std::os::unix::ffi::OsStrExt;

const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeDirIssue {
    NotDirectory,
    Symlink,
    InsecureTempDir,
    WrongOwner { expected: u32, actual: u32 },
    OwnerPermissionsInsufficient { mode: u32 },
    PermissionsTooOpen { mode: u32 },
}

impl std::fmt::Display for RuntimeDirIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDirectory => write!(f, "path is not a directory"),
            Self::Symlink => write!(f, "path must not be a symlink"),
            Self::InsecureTempDir => {
                write!(f, "refusing to use /tmp as a trusted runtime directory")
            }
            Self::WrongOwner { expected, actual } => {
                write!(
                    f,
                    "directory is owned by uid {actual}, expected uid {expected}"
                )
            }
            Self::OwnerPermissionsInsufficient { mode } => write!(
                f,
                "directory permissions {:04o} are not usable; require owner rwx access (0700-style)",
                mode
            ),
            Self::PermissionsTooOpen { mode } => write!(
                f,
                "directory permissions {:04o} are too open; require owner-only access (0700-style)",
                mode
            ),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct RuntimeDirResolution {
    pub(super) dir: Option<PathBuf>,
    pub(super) warnings: Vec<String>,
}

pub(super) fn socket_path_in(dir: &Path) -> PathBuf {
    let pid = std::process::id();
    // Keep the basename fixed-size: Linux sockaddr_un paths are short, while
    // the full digest still prevents two named sessions from aliasing.
    let name = match instance::session_digest_key() {
        Some(digest) => format!("taarof-s-{digest}-{pid}.sock"),
        None => format!("taarof-{pid}.sock"),
    };
    dir.join(name)
}

pub(super) fn validate_socket_path_length(path: &Path) -> Result<(), String> {
    let bytes = path.as_os_str().as_bytes().len();
    (bytes <= MAX_UNIX_SOCKET_PATH_BYTES)
        .then_some(())
        .ok_or_else(|| {
            format!("socket path is {bytes} bytes; maximum is {MAX_UNIX_SOCKET_PATH_BYTES}")
        })
}

pub(super) fn select_runtime_dir(
    xdg_configured: bool,
    xdg_runtime_dir: Option<PathBuf>,
    run_user_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if xdg_configured {
        xdg_runtime_dir
    } else {
        run_user_dir
    }
}

pub(super) fn insecure_temp_runtime_dir() -> PathBuf {
    Path::new("/").join("tmp")
}

pub(super) fn validate_runtime_dir_attributes(
    path: &Path,
    owner_uid: u32,
    mode: u32,
    is_dir: bool,
    current_uid: u32,
) -> Result<(), RuntimeDirIssue> {
    if !is_dir {
        return Err(RuntimeDirIssue::NotDirectory);
    }
    if path == insecure_temp_runtime_dir() {
        return Err(RuntimeDirIssue::InsecureTempDir);
    }
    if owner_uid != current_uid {
        return Err(RuntimeDirIssue::WrongOwner {
            expected: current_uid,
            actual: owner_uid,
        });
    }
    let mode = mode & 0o777;
    if mode & 0o700 != 0o700 {
        return Err(RuntimeDirIssue::OwnerPermissionsInsufficient { mode });
    }
    if mode & 0o077 != 0 {
        return Err(RuntimeDirIssue::PermissionsTooOpen { mode });
    }
    Ok(())
}

pub(super) fn validate_runtime_dir(path: &Path, current_uid: u32) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("could not inspect runtime directory: {e}"))?;
    if metadata.file_type().is_symlink() {
        return Err(RuntimeDirIssue::Symlink.to_string());
    }
    validate_runtime_dir_attributes(
        path,
        metadata.uid(),
        metadata.mode(),
        metadata.is_dir(),
        current_uid,
    )
    .map_err(|issue| issue.to_string())
}

pub(super) fn runtime_dir_resolution() -> RuntimeDirResolution {
    let current_uid = unsafe { libc::getuid() } as u32;
    let mut resolution = RuntimeDirResolution::default();

    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let xdg_configured = xdg_runtime_dir.is_some();

    let xdg_runtime_dir = xdg_runtime_dir.and_then(|path| match validate_runtime_dir(&path, current_uid)
    {
        Ok(()) => Some(path),
        Err(reason) => {
            resolution.warnings.push(format!(
                "taarof: ignoring unsafe XDG_RUNTIME_DIR {}: {reason}; socket server disabled instead of falling back",
                path.display()
            ));
            None
        }
    });

    let run_user_dir = if xdg_configured {
        None
    } else {
        // Fallback: /run/user/{uid}/ (standard Linux path)
        let path = PathBuf::from(format!("/run/user/{current_uid}"));
        if path.exists() {
            match validate_runtime_dir(&path, current_uid) {
                Ok(()) => Some(path),
                Err(reason) => {
                    resolution.warnings.push(format!(
                        "taarof: ignoring unsafe fallback runtime dir {}: {reason}",
                        path.display()
                    ));
                    None
                }
            }
        } else {
            None
        }
    };

    resolution.dir = select_runtime_dir(xdg_configured, xdg_runtime_dir, run_user_dir);
    resolution
}
