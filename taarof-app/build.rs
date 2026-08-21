//! Build-time source provenance.  This intentionally only trusts the repository
//! that contains this crate; a parent checkout must never lend an export a Git
//! identity by accident.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Provenance {
    root: Option<PathBuf>,
    revision: Option<String>,
    describe: Option<String>,
    dirty: Option<bool>,
}

fn main() {
    for key in [
        "TAAROF_SOURCE_REVISION",
        "TAAROF_SOURCE_DESCRIBE",
        "TAAROF_SOURCE_DIRTY",
        "TAAROF_SOURCE_ROOT",
        "TAAROF_REPRODUCIBLE_BUILD",
        "SOURCE_DATE_EPOCH",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let provenance = discover(&manifest_dir);
    if let Some(root) = provenance.root.as_deref() {
        emit_git_rerun_contract(root);
    }

    let built_at = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|value| value.trim().parse::<u64>().is_ok())
        .map(|value| value.trim().to_string())
        .or_else(|| git(provenance.root.as_deref(), &["log", "-1", "--format=%ct"]));
    let profile = std::env::var("PROFILE").ok();
    let build_id = build_id(&provenance, profile.as_deref(), built_at.as_deref());
    let reproducible_build = required_env_bool("TAAROF_REPRODUCIBLE_BUILD").unwrap_or(false);
    let recorded_source_root = (!reproducible_build)
        .then_some(provenance.root.as_deref())
        .flatten();

    emit(
        "TAAROF_BUILD_SOURCE_REVISION",
        provenance.revision.as_deref(),
    );
    emit(
        "TAAROF_BUILD_SOURCE_DESCRIBE",
        provenance.describe.as_deref(),
    );
    emit("TAAROF_BUILD_SOURCE_DIRTY", provenance.dirty.map(bool_word));
    emit(
        "TAAROF_BUILD_SOURCE_ROOT",
        recorded_source_root.map(path_string).as_deref(),
    );
    emit("TAAROF_BUILD_SOURCE_BUILT_AT_UNIX", built_at.as_deref());
    emit("TAAROF_BUILD_PROFILE", profile.as_deref());
    emit("TAAROF_BUILD_ID", Some(&build_id));

    // A release-side helper turns this stamp into a SHA-bound sidecar after
    // linking. OUT_DIR is tied to this exact Cargo build, never the installer.
    let stamp = format!(
        "{{\"schema\":\"taarof.build-stamp.v1\",\"app_version\":\"{}\",\"build_id\":{},\"source_revision\":{},\"source_describe\":{},\"source_dirty\":{},\"source_root\":{},\"profile\":{},\"built_at_unix\":{}}}",
        std::env::var("CARGO_PKG_VERSION").unwrap(), json(&build_id), json_opt(provenance.revision.as_deref()), json_opt(provenance.describe.as_deref()), provenance.dirty.map(bool_word).unwrap_or("null"), json_opt(recorded_source_root.map(path_string).as_deref()), json_opt(profile.as_deref()), json_opt(built_at.as_deref())
    );
    let _ = std::fs::write(
        PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("taarof-build-stamp.json"),
        stamp,
    );
}

fn discover(manifest_dir: &Path) -> Provenance {
    let overrides = [
        "TAAROF_SOURCE_ROOT",
        "TAAROF_SOURCE_REVISION",
        "TAAROF_SOURCE_DESCRIBE",
        "TAAROF_SOURCE_DIRTY",
    ];
    let any_override = overrides.iter().any(|key| std::env::var_os(key).is_some());
    if any_override {
        // Overrides are an atomic provenance assertion. Mixing one override
        // with an ambient repository was the path that mislabelled exports.
        let Some(root) = env_override("TAAROF_SOURCE_ROOT").map(PathBuf::from) else {
            return unknown();
        };
        let (Some(revision), Some(describe), Some(dirty)) = (
            env_override("TAAROF_SOURCE_REVISION"),
            env_override("TAAROF_SOURCE_DESCRIBE"),
            env_bool("TAAROF_SOURCE_DIRTY"),
        ) else {
            return unknown();
        };
        let Some(root) = intended_git_root(&root) else {
            return unknown();
        };
        return Provenance {
            root: Some(root),
            revision: Some(revision),
            describe: Some(describe),
            dirty: Some(dirty),
        };
    }

    // taarof-app is directly beneath the intended repository root. Do not ask
    // Git to walk upward from an extracted/nested directory.
    let intended = manifest_dir.parent().unwrap_or(manifest_dir);
    let Some(root) = intended_git_root(intended) else {
        return unknown();
    };
    Provenance {
        revision: git(Some(&root), &["rev-parse", "HEAD"]),
        describe: git(Some(&root), &["describe", "--tags", "--always", "--dirty"]),
        // Untracked inputs are source inputs for provenance too.
        dirty: git_output(
            Some(&root),
            &["status", "--porcelain", "--untracked-files=all"],
        )
        .map(|v| !v.is_empty()),
        root: Some(root),
    }
}

fn unknown() -> Provenance {
    Provenance {
        root: None,
        revision: None,
        describe: None,
        dirty: None,
    }
}

fn intended_git_root(intended: &Path) -> Option<PathBuf> {
    if !intended.is_dir() {
        return None;
    }
    let top = git(Some(intended), &["rev-parse", "--show-toplevel"])?;
    let intended = intended.canonicalize().ok()?;
    let top = PathBuf::from(top).canonicalize().ok()?;
    (top == intended).then_some(top)
}

fn emit_git_rerun_contract(root: &Path) {
    // Directory tracking catches clean<->dirty changes including untracked
    // files; the git paths make commits and staged changes immediate and are
    // resolved by Git so linked worktrees use their real common gitdir.
    println!("cargo:rerun-if-changed={}", root.display());
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path(root, path) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(reference) = git(Some(root), &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_path(root, &reference) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn git_path(root: &Path, path: &str) -> Option<PathBuf> {
    let value = git(Some(root), &["rev-parse", "--git-path", path])?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn build_id(provenance: &Provenance, profile: Option<&str>, built_at: Option<&str>) -> String {
    // This is an opaque *binding token*, not a security hash. It is embedded in
    // the executable and must be present in a sidecar before that sidecar can
    // make source claims about the artifact.
    format!(
        "taarof-build-v2:{}:{}:{}:{}",
        provenance.revision.as_deref().unwrap_or("unknown"),
        provenance.dirty.map(bool_word).unwrap_or("unknown"),
        profile.unwrap_or("unknown"),
        built_at.unwrap_or("unknown")
    )
}

fn bool_word(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn emit(key: &str, value: Option<&str>) {
    println!("cargo:rustc-env={key}={}", value.unwrap_or(""));
}
fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
fn env_bool(key: &str) -> Option<bool> {
    match env_override(key)?.as_str() {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}
fn required_env_bool(key: &str) -> Option<bool> {
    let value = std::env::var_os(key)?;
    env_bool(key).or_else(|| panic!("{} must be true or false when set, got {:?}", key, value))
}
fn json(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
fn json_opt(value: Option<&str>) -> String {
    value.map(json).unwrap_or_else(|| "null".into())
}

fn git(root: Option<&Path>, args: &[&str]) -> Option<String> {
    git_output(root, args).filter(|value| !value.is_empty())
}

fn git_output(root: Option<&Path>, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root?)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn temp(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "taarof-build-{name}-{}",
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn run(dir: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    #[test]
    fn real_git_clean_and_untracked_dirty_are_distinguished() {
        let root = temp("clean-dirty");
        fs::create_dir_all(root.join("taarof-app")).unwrap();
        run(&root, &["init"]);
        run(&root, &["config", "user.email", "test@example.invalid"]);
        run(&root, &["config", "user.name", "test"]);
        fs::write(root.join("tracked"), "x").unwrap();
        fs::write(root.join("taarof-app/lib.rs"), "x").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "initial"]);
        let clean = discover(&root.join("taarof-app"));
        assert_eq!(clean.dirty, Some(false));
        assert!(clean.revision.is_some());
        fs::write(root.join("untracked-build-input"), "x").unwrap();
        assert_eq!(discover(&root.join("taarof-app")).dirty, Some(true));
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn nested_git_free_export_is_unknown_even_below_a_git_repo() {
        let root = temp("nested-export");
        run(&root, &["init"]);
        fs::create_dir_all(root.join("export/taarof-app")).unwrap();
        assert_eq!(discover(&root.join("export/taarof-app")), unknown());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn reproducible_build_id_does_not_depend_on_checkout_location() {
        let first = Provenance {
            root: Some(PathBuf::from("/tmp/first-public-export")),
            revision: Some("0123456789abcdef".into()),
            describe: Some("0123456".into()),
            dirty: Some(false),
        };
        let second = Provenance {
            root: Some(PathBuf::from("/tmp/second-public-export")),
            ..first.clone()
        };

        assert_eq!(
            build_id(&first, Some("release"), Some("1704067200")),
            build_id(&second, Some("release"), Some("1704067200"))
        );
    }
    #[test]
    fn linked_worktree_paths_include_head_index_and_ref() {
        let root = temp("worktree");
        run(&root, &["init"]);
        run(&root, &["config", "user.email", "test@example.invalid"]);
        run(&root, &["config", "user.name", "test"]);
        fs::write(root.join("x"), "x").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-m", "initial"]);
        let linked = temp("linked");
        run(
            &root,
            &["worktree", "add", "-b", "other", linked.to_str().unwrap()],
        );
        let top = intended_git_root(&linked).unwrap();
        assert!(git_path(&top, "HEAD").unwrap().exists());
        assert!(git_path(&top, "index").unwrap().exists());
        let reference = git(Some(&top), &["symbolic-ref", "-q", "HEAD"]).unwrap();
        assert!(git_path(&top, &reference).unwrap().exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(linked);
    }
}
