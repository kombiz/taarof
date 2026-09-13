//! Local Git repository identity shared by the primary checkout and linked worktrees.
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Resolve metadata only: no Git process, network access, or branch mutation.
/// The nearest Git boundary wins, including when that boundary is unreadable.
pub fn repository_common_dir(cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().ok()?;
    if !cwd.is_dir() {
        return None;
    }
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
        let git_dir = if marker.is_dir() {
            marker
        } else {
            let pointer = read_pointer(&marker)?;
            ancestor.join(pointer.strip_prefix("gitdir: ")?)
        };
        let git_dir = git_dir.canonicalize().ok()?;
        if !git_dir.join("HEAD").is_file() {
            return None;
        }
        let common_pointer = git_dir.join("commondir");
        let common = match std::fs::symlink_metadata(&common_pointer) {
            Ok(_) => git_dir.join(read_pointer(&common_pointer)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir,
            Err(_) => return None,
        };
        let common = common.canonicalize().ok()?;
        return common.join("objects").is_dir().then_some(common);
    }
    None
}

fn read_pointer(path: &Path) -> Option<String> {
    const LIMIT: u64 = 8192;
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut value = String::new();
    file.take(LIMIT + 1).read_to_string(&mut value).ok()?;
    if value.len() as u64 > LIMIT {
        return None;
    }
    let value = value.trim_end_matches(['\r', '\n']);
    (!value.is_empty()).then(|| value.to_owned())
}
