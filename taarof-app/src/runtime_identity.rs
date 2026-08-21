//! Trustworthy runtime and source build identity (EXAMPLE-164).
//!
//! The failure this module exists to prevent: the installed binary and the
//! freshly built binary hashed identically, so every surface reported
//! `current` — while *both* predated the source fix under test. Byte equality
//! between two binaries says nothing about whether either was built from the
//! source in front of you.
//!
//! So identity answers two independent questions and never conflates them:
//!
//! * `binary_state` — is the running binary the installed one? (owned by
//!   [`crate::update_watch`], which hashes both.)
//! * `source_state` — was the running binary built from *this* checkout?
//!
//! The second one is [`SourceState::Unknown`] unless it can be positively
//! proved. Missing provenance, an unreadable checkout, a dirty tree, or a
//! revision we cannot pin all degrade to `Unknown`; none of them are ever
//! rounded up to `Matches`. An honest "I don't know" is the entire product
//! here, because a confident wrong answer is what cost the original debugging
//! session.
//!
//! All discovery — `git` invocation, manifest reads — runs on the blocking
//! worker owned by [`crate::update_watch::spawn_refresh`]. Nothing in this
//! module may be called from a GTK callback except [`cached`] and
//! [`build_identity`], which only read memory.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::update_watch::{BinaryIdentity, UpdateState, UpdateStatus};

pub const IDENTITY_SCHEMA: &str = "taarof.identity.v1";
pub const INSTALL_MANIFEST_SCHEMA: &str = "taarof.artifact.v1";

/// Path of the install manifest relative to an install prefix.
const INSTALL_MANIFEST_RELATIVE: &str = "share/taarof/install-manifest.json";

/// Build-time provenance stamped in by `build.rs`. An empty compile-time string
/// means the builder could not determine the value; it becomes `None` here and
/// `null` on the wire. It never becomes a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildIdentity {
    pub build_id: Option<String>,
    pub source_revision: Option<String>,
    pub source_describe: Option<String>,
    pub source_dirty: Option<bool>,
    pub source_root: Option<String>,
    pub profile: Option<String>,
    pub built_at_unix_ms: Option<u64>,
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub fn build_identity() -> BuildIdentity {
    BuildIdentity {
        build_id: non_empty(env!("TAAROF_BUILD_ID")),
        source_revision: non_empty(env!("TAAROF_BUILD_SOURCE_REVISION")),
        source_describe: non_empty(env!("TAAROF_BUILD_SOURCE_DESCRIBE")),
        source_dirty: parse_bool(env!("TAAROF_BUILD_SOURCE_DIRTY")),
        source_root: non_empty(env!("TAAROF_BUILD_SOURCE_ROOT")),
        profile: non_empty(env!("TAAROF_BUILD_PROFILE")),
        built_at_unix_ms: non_empty(env!("TAAROF_BUILD_SOURCE_BUILT_AT_UNIX"))
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1000)),
    }
}

/// What the checkout on disk says *right now*.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceIdentity {
    pub root: Option<String>,
    pub revision: Option<String>,
    pub describe: Option<String>,
    pub dirty: Option<bool>,
    pub error: Option<String>,
}

impl SourceIdentity {
    fn unavailable(root: Option<String>, error: &str) -> Self {
        Self {
            root,
            error: Some(error.to_string()),
            ..Self::default()
        }
    }
}

/// Provenance recorded by the installer at the moment it copied the binary in.
/// This is what makes "installed, but installed from *what*" answerable without
/// re-deriving history the installed tree no longer has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InstalledProvenance {
    pub manifest_path: Option<String>,
    pub app_version: Option<String>,
    pub source_revision: Option<String>,
    pub source_describe: Option<String>,
    pub source_dirty: Option<bool>,
    pub build_id: Option<String>,
    pub binary_sha256: Option<String>,
    pub installed_at_unix_ms: Option<u64>,
    pub error: Option<String>,
}

/// Whether the running binary was built from the checkout we can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// Positively proved: both revisions known, equal, and both trees clean.
    Matches,
    /// Positively disproved: both revisions known and different.
    Differs,
    /// Not provable. The default, and the only honest answer when anything is
    /// missing, unreadable, or uncommitted.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeIdentity {
    pub schema: &'static str,
    pub app_version: &'static str,
    pub build: BuildIdentity,
    pub source: SourceIdentity,
    pub installed_provenance: Option<InstalledProvenance>,
    pub running: BinaryIdentity,
    pub installed: BinaryIdentity,
    /// Tri-state on purpose: `None` means we could not hash both sides, which
    /// is different from "we hashed both and they differ".
    pub running_matches_installed: Option<bool>,
    pub source_state: SourceState,
    pub binary_state: UpdateState,
    pub checked_at_unix_ms: Option<u64>,
}

impl RuntimeIdentity {
    /// The identity of a process that has not completed a probe yet. Every
    /// verdict is unknown; nothing here can be mistaken for a fresh build.
    pub fn not_checked() -> Self {
        Self {
            schema: IDENTITY_SCHEMA,
            app_version: env!("CARGO_PKG_VERSION"),
            build: build_identity(),
            source: SourceIdentity::default(),
            installed_provenance: None,
            running: BinaryIdentity::unknown("not_checked"),
            installed: BinaryIdentity::unknown("not_checked"),
            running_matches_installed: None,
            source_state: SourceState::Unknown,
            binary_state: UpdateState::Unknown,
            checked_at_unix_ms: None,
        }
    }

    /// Short provenance token for compact UI. Never claims freshness.
    pub fn short_source_label(&self) -> String {
        match (&self.build.source_describe, &self.build.source_revision) {
            (Some(describe), _) => describe.clone(),
            (None, Some(revision)) => revision.chars().take(12).collect(),
            (None, None) => "unknown source".to_string(),
        }
    }
}

/// The core rule, kept pure so it can be tested without a filesystem.
///
/// `Matches` requires *proof*: both revisions present, equal, and both trees
/// known-clean. A dirty tree on either side means the revision no longer
/// identifies the bytes, so the answer degrades to `Unknown` rather than
/// pretending the commit id still describes the build.
pub fn evaluate_source_state(
    build_revision: Option<&str>,
    build_dirty: Option<bool>,
    source_revision: Option<&str>,
    source_dirty: Option<bool>,
) -> SourceState {
    let (Some(build_revision), Some(source_revision)) = (build_revision, source_revision) else {
        return SourceState::Unknown;
    };
    if build_revision != source_revision {
        return SourceState::Differs;
    }
    match (build_dirty, source_dirty) {
        (Some(false), Some(false)) => SourceState::Matches,
        _ => SourceState::Unknown,
    }
}

fn cache() -> &'static Mutex<Option<RuntimeIdentity>> {
    static CACHE: OnceLock<Mutex<Option<RuntimeIdentity>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Main-thread safe: clones the last completed probe, or an all-unknown
/// identity when none has completed.
pub fn cached() -> RuntimeIdentity {
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_else(RuntimeIdentity::not_checked)
}

pub(crate) fn store(identity: RuntimeIdentity) {
    *cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(identity);
}

/// Build the full identity. **Blocking**: runs `git` and reads the install
/// manifest. Callers must already be on the update-watch blocking worker.
pub(crate) fn compute(status: &UpdateStatus) -> RuntimeIdentity {
    let build = build_identity();
    let source = discover_source(build.source_root.as_deref().map(Path::new));
    let installed_provenance = status
        .installed
        .path
        .as_deref()
        .map(Path::new)
        .and_then(|path| read_install_manifest(path, &status.installed, &build));
    let source_state = evaluate_source_state(
        build.source_revision.as_deref(),
        build.source_dirty,
        source.revision.as_deref(),
        source.dirty,
    );

    RuntimeIdentity {
        schema: IDENTITY_SCHEMA,
        app_version: env!("CARGO_PKG_VERSION"),
        build,
        source,
        installed_provenance,
        running: status.running.clone(),
        installed: status.installed.clone(),
        running_matches_installed: status.content_matches,
        source_state,
        binary_state: status.state,
        checked_at_unix_ms: status.checked_at_unix_ms,
    }
}

/// Blocking. Ask the checkout what it currently is.
fn discover_source(root: Option<&Path>) -> SourceIdentity {
    let Some(root) = root else {
        return SourceIdentity::unavailable(None, "no_source_root_recorded");
    };
    let reported = root.to_string_lossy().into_owned();
    if !root.is_dir() {
        // Normal for an installed binary on a host without the checkout. Not an
        // error, but definitively not a proof of freshness either.
        return SourceIdentity::unavailable(Some(reported), "source_root_missing");
    }

    // Do not let `git -C` walk out of an exported/nested directory. The build
    // recorded an intended repository root, and that exact directory must
    // still be Git's top-level root before it can prove anything.
    let Some(top_level) = git(root, &["rev-parse", "--show-toplevel"]) else {
        return SourceIdentity::unavailable(Some(reported), "git_unavailable");
    };
    let expected = root.canonicalize().ok();
    let actual = PathBuf::from(top_level).canonicalize().ok();
    if expected.is_none() || actual != expected {
        return SourceIdentity::unavailable(Some(reported), "source_root_not_git_toplevel");
    }
    let Some(revision) = git(root, &["rev-parse", "HEAD"]) else {
        return SourceIdentity::unavailable(Some(reported), "git_unavailable");
    };

    SourceIdentity {
        root: Some(reported),
        revision: Some(revision),
        describe: git(root, &["describe", "--tags", "--always", "--dirty"]),
        dirty: git_output(root, &["status", "--porcelain", "--untracked-files=all"])
            .map(|output| !output.trim().is_empty()),
        error: None,
    }
}

/// Blocking. `git` is read-only here and its absence is a normal outcome.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    // Not a pane spawn: no PTY, no child_env seam, and `output()` reaps the
    // child itself, so `spawn_and_reap` does not apply.
    git_output(root, args).filter(|value| !value.is_empty())
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    Some(value)
}

/// `<prefix>/bin/taarof-app` → `<prefix>/share/taarof/install-manifest.json`.
pub fn install_manifest_path(installed_binary: &Path) -> Option<PathBuf> {
    let prefix = installed_binary.parent()?.parent()?;
    Some(prefix.join(INSTALL_MANIFEST_RELATIVE))
}

/// Blocking. A missing manifest is reported, not silently treated as agreement.
fn read_install_manifest(
    installed_binary: &Path,
    installed: &BinaryIdentity,
    build: &BuildIdentity,
) -> Option<InstalledProvenance> {
    let path = install_manifest_path(installed_binary)?;
    let reported = path.to_string_lossy().into_owned();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Some(InstalledProvenance {
            manifest_path: Some(reported),
            error: Some("manifest_missing".to_string()),
            ..InstalledProvenance::default()
        });
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Some(InstalledProvenance {
            manifest_path: Some(reported),
            error: Some("manifest_unparseable".to_string()),
            ..InstalledProvenance::default()
        });
    };
    Some(parse_install_manifest(
        Some(reported),
        &value,
        installed,
        build,
    ))
}

fn parse_install_manifest(
    manifest_path: Option<String>,
    value: &serde_json::Value,
    installed: &BinaryIdentity,
    build: &BuildIdentity,
) -> InstalledProvenance {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(non_empty)
    };
    let schema_ok =
        value.get("schema").and_then(serde_json::Value::as_str) == Some(INSTALL_MANIFEST_SCHEMA);
    let app_version = string("app_version");
    let binary_sha256 = string("binary_sha256");
    let build_id = string("build_id");
    let source_revision = string("source_revision");
    let source_describe = string("source_describe");
    let source_dirty = value
        .get("source_dirty")
        .and_then(serde_json::Value::as_bool);
    let valid_hash = |value: Option<&str>| {
        value.is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    };
    let binary_matches = valid_hash(binary_sha256.as_deref())
        && valid_hash(installed.sha256.as_deref())
        && binary_sha256 == installed.sha256;
    let build_matches = build_id.is_some() && build_id == build.build_id;
    let version_matches = app_version.as_deref() == Some(env!("CARGO_PKG_VERSION"));
    let source_matches = source_revision == build.source_revision
        && source_describe == build.source_describe
        && source_dirty == build.source_dirty;
    if !schema_ok || !binary_matches || !build_matches || !version_matches || !source_matches {
        return InstalledProvenance {
            manifest_path,
            error: Some(
                if !schema_ok {
                    "manifest_schema_unrecognized"
                } else if !binary_matches {
                    "manifest_binary_mismatch"
                } else if !version_matches {
                    "manifest_version_mismatch"
                } else if !source_matches {
                    "manifest_provenance_mismatch"
                } else {
                    "manifest_build_mismatch"
                }
                .to_string(),
            ),
            ..InstalledProvenance::default()
        };
    }
    InstalledProvenance {
        manifest_path,
        app_version,
        source_revision,
        source_describe,
        source_dirty,
        build_id,
        binary_sha256,
        installed_at_unix_ms: value
            .get("installed_at_unix")
            .and_then(serde_json::Value::as_u64)
            .map(|seconds| seconds.saturating_mul(1000)),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "taarof-runtime-identity-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn matching_revisions_on_clean_trees_are_the_only_proved_match() {
        let revision = "c".repeat(40);
        assert_eq!(
            evaluate_source_state(Some(&revision), Some(false), Some(&revision), Some(false)),
            SourceState::Matches
        );
    }

    #[test]
    fn absent_build_provenance_is_unknown_never_matching() {
        let revision = "c".repeat(40);
        // The motivating regression: the binary is byte-identical to what is
        // installed, and the checkout is clean and readable — but the build
        // never recorded what it came from, so nothing is proved.
        assert_eq!(
            evaluate_source_state(None, None, Some(&revision), Some(false)),
            SourceState::Unknown
        );
        // ...and symmetrically when the checkout is the unreadable side.
        assert_eq!(
            evaluate_source_state(Some(&revision), Some(false), None, None),
            SourceState::Unknown
        );
        assert_eq!(
            evaluate_source_state(None, None, None, None),
            SourceState::Unknown
        );
    }

    #[test]
    fn differing_revisions_are_disproved_not_unknown() {
        assert_eq!(
            evaluate_source_state(
                Some(&"a".repeat(40)),
                Some(false),
                Some(&"b".repeat(40)),
                Some(false)
            ),
            SourceState::Differs
        );
        // A disproof survives dirtiness: the commits genuinely differ.
        assert_eq!(
            evaluate_source_state(
                Some(&"a".repeat(40)),
                Some(true),
                Some(&"b".repeat(40)),
                None
            ),
            SourceState::Differs
        );
    }

    #[test]
    fn dirty_or_unknown_cleanliness_downgrades_an_equal_revision() {
        let revision = "c".repeat(40);
        for (build_dirty, source_dirty) in [
            (Some(true), Some(false)),
            (Some(false), Some(true)),
            (Some(true), Some(true)),
            (None, Some(false)),
            (Some(false), None),
        ] {
            assert_eq!(
                evaluate_source_state(Some(&revision), build_dirty, Some(&revision), source_dirty),
                SourceState::Unknown,
                "equal revision with dirty={build_dirty:?}/{source_dirty:?} must not claim a match"
            );
        }
    }

    #[test]
    fn identical_binaries_with_unknown_source_report_current_but_not_matching() {
        let hash = "a".repeat(64);
        let binary = |path: &str| BinaryIdentity {
            path: Some(path.to_string()),
            resolved_path: Some(path.to_string()),
            symlink: false,
            deleted: false,
            size_bytes: Some(4),
            sha256: Some(hash.clone()),
            build_id: None,
            readable: true,
            error: None,
        };
        let status = UpdateStatus::new(
            UpdateState::Current,
            None,
            binary("/proc/self/exe"),
            binary("/usr/local/bin/taarof-app"),
            Some(7),
        );
        let mut identity = compute(&status);
        // Force the "build recorded nothing" case regardless of how this test
        // binary itself was built (in-tree it really does carry provenance).
        identity.build.source_revision = None;
        identity.build.source_describe = None;
        identity.build.source_dirty = None;
        identity.source_state = evaluate_source_state(
            identity.build.source_revision.as_deref(),
            identity.build.source_dirty,
            identity.source.revision.as_deref(),
            identity.source.dirty,
        );

        assert_eq!(identity.binary_state, UpdateState::Current);
        assert_eq!(identity.running_matches_installed, Some(true));
        assert_eq!(identity.source_state, SourceState::Unknown);
        assert_eq!(identity.short_source_label(), "unknown source");
    }

    #[test]
    fn not_checked_identity_is_unknown_on_every_axis() {
        let identity = RuntimeIdentity::not_checked();
        assert_eq!(identity.source_state, SourceState::Unknown);
        assert_eq!(identity.binary_state, UpdateState::Unknown);
        assert_eq!(identity.running_matches_installed, None);
        assert_eq!(identity.schema, IDENTITY_SCHEMA);
        assert_eq!(identity.app_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn serialized_identity_matches_the_documented_wire_contract() {
        let identity = RuntimeIdentity::not_checked();
        let value = serde_json::to_value(&identity).expect("identity should serialize");
        for key in [
            "schema",
            "app_version",
            "build",
            "source",
            "installed_provenance",
            "running",
            "installed",
            "running_matches_installed",
            "source_state",
            "binary_state",
            "checked_at_unix_ms",
        ] {
            assert!(value.get(key).is_some(), "identity must expose {key}");
        }
        assert_eq!(value["schema"], IDENTITY_SCHEMA);
        assert_eq!(value["source_state"], "unknown");
        assert_eq!(value["binary_state"], "unknown");
        assert!(value["running_matches_installed"].is_null());
    }

    #[test]
    fn install_manifest_path_is_derived_from_the_install_prefix() {
        assert_eq!(
            install_manifest_path(Path::new("/tmp/user/.local/bin/taarof-app")),
            Some(PathBuf::from(
                "/tmp/user/.local/share/taarof/install-manifest.json"
            ))
        );
        assert_eq!(install_manifest_path(Path::new("taarof-app")), None);
    }

    #[test]
    fn install_manifest_is_read_and_a_foreign_schema_is_flagged() {
        let dir = temp_dir("manifest");
        let bin = dir.join("bin");
        let share = dir.join("share/taarof");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&share).unwrap();
        let binary = bin.join("taarof-app");
        std::fs::write(&binary, b"x").unwrap();
        let build = build_identity();
        let installed = BinaryIdentity {
            path: Some(binary.to_string_lossy().into_owned()),
            resolved_path: Some(binary.to_string_lossy().into_owned()),
            symlink: false,
            deleted: false,
            size_bytes: Some(1),
            sha256: Some(
                "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".to_string(),
            ),
            build_id: None,
            readable: true,
            error: None,
        };

        std::fs::write(
            share.join("install-manifest.json"),
            serde_json::json!({
                "schema": INSTALL_MANIFEST_SCHEMA,
                "app_version": env!("CARGO_PKG_VERSION"),
                "build_id": build.build_id,
                "source_revision": build.source_revision,
                "source_describe": build.source_describe,
                "source_dirty": build.source_dirty,
                "binary_sha256": installed.sha256,
                "installed_at_unix": 1_700_000_000_u64,
            })
            .to_string(),
        )
        .unwrap();
        let provenance =
            read_install_manifest(&binary, &installed, &build).expect("manifest should be read");
        assert_eq!(provenance.source_revision, build.source_revision);
        assert_eq!(provenance.source_dirty, build.source_dirty);
        assert_eq!(provenance.installed_at_unix_ms, Some(1_700_000_000_000));
        assert_eq!(provenance.error, None);

        std::fs::write(
            share.join("install-manifest.json"),
            r#"{"schema":"something.else.v9"}"#,
        )
        .unwrap();
        let foreign =
            read_install_manifest(&binary, &installed, &build).expect("manifest should be read");
        assert_eq!(
            foreign.error.as_deref(),
            Some("manifest_schema_unrecognized")
        );
        assert_eq!(foreign.source_revision, None);

        std::fs::remove_file(share.join("install-manifest.json")).unwrap();
        let missing = read_install_manifest(&binary, &installed, &build)
            .expect("absence is still reportable");
        assert_eq!(missing.error.as_deref(), Some("manifest_missing"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replacement_or_tampered_manifest_downgrades_every_provenance_field() {
        let build = build_identity();
        let installed = BinaryIdentity {
            path: Some("/tmp/taarof-app".to_string()),
            resolved_path: None,
            symlink: false,
            deleted: false,
            size_bytes: Some(1),
            sha256: Some("a".repeat(64)),
            build_id: None,
            readable: true,
            error: None,
        };
        let value = serde_json::json!({
            "schema": INSTALL_MANIFEST_SCHEMA, "app_version": env!("CARGO_PKG_VERSION"),
            "build_id": build.build_id, "source_revision": "c".repeat(40),
            "source_dirty": false, "binary_sha256": "b".repeat(64),
        });
        let rejected = parse_install_manifest(
            Some("/tmp/manifest".to_string()),
            &value,
            &installed,
            &build,
        );
        assert_eq!(rejected.error.as_deref(), Some("manifest_binary_mismatch"));
        assert_eq!(rejected.source_revision, None);
        assert_eq!(rejected.app_version, None);
        assert_eq!(rejected.build_id, None);
        assert_eq!(rejected.binary_sha256, None);

        let mut tampered = value;
        tampered["binary_sha256"] = serde_json::Value::String("a".repeat(64));
        tampered["source_revision"] = serde_json::Value::String("f".repeat(40));
        let rejected = parse_install_manifest(
            Some("/tmp/manifest".to_string()),
            &tampered,
            &installed,
            &build,
        );
        assert_eq!(
            rejected.error.as_deref(),
            Some("manifest_provenance_mismatch")
        );
        assert_eq!(rejected.source_revision, None);
    }

    #[test]
    fn source_discovery_reports_a_missing_checkout_instead_of_guessing() {
        assert_eq!(
            discover_source(None).error.as_deref(),
            Some("no_source_root_recorded")
        );
        let dir = temp_dir("no-checkout");
        let absent = dir.join("gone");
        let identity = discover_source(Some(&absent));
        assert_eq!(identity.error.as_deref(), Some("source_root_missing"));
        assert_eq!(identity.revision, None);
        assert_eq!(
            evaluate_source_state(Some(&"a".repeat(40)), Some(false), None, None),
            SourceState::Unknown
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn short_source_label_prefers_describe_then_revision() {
        let mut identity = RuntimeIdentity::not_checked();
        // An in-tree build stamps real provenance, so clear it explicitly to
        // exercise the "nothing recorded" branch.
        identity.build.source_revision = None;
        identity.build.source_describe = None;
        assert_eq!(identity.short_source_label(), "unknown source");
        identity.build.source_revision = Some("abcdef1234567890".to_string());
        assert_eq!(identity.short_source_label(), "abcdef123456");
        identity.build.source_describe = Some("v0.1.0-2-gabcdef1".to_string());
        assert_eq!(identity.short_source_label(), "v0.1.0-2-gabcdef1");
    }
}
