//! Web asset bundle lookup and static file serving.

use super::*;

// ── Static web assets ──

#[derive(Clone, Debug)]
pub(super) struct ResolvedWebAssets {
    pub(super) dist_dir: PathBuf,
    pub(super) index_path: PathBuf,
}

pub(super) fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|existing| existing == &candidate) {
        paths.push(candidate);
    }
}

pub(super) fn repo_root_dir() -> PathBuf {
    let recorded_root = env!("TAAROF_BUILD_SOURCE_ROOT").trim();
    if !recorded_root.is_empty() {
        return PathBuf::from(recorded_root);
    }

    // Reproducible release artifacts intentionally do not embed the builder's
    // absolute checkout, and provenance is also absent when Git refuses the
    // checkout (a container build over a differently-owned workspace). Installed
    // layout and XDG candidates remain authoritative; recover the source tree
    // from the running executable so a `cargo run`/`cargo test` binary still
    // resolves `taarof-web/dist` regardless of the working directory, and treat
    // the working directory as the last-resort convenience fallback.
    std::env::current_exe()
        .ok()
        .and_then(|exe_path| source_checkout_root_from_exe_path(&exe_path))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// Cargo places built binaries under `<repo>/taarof-app/target/...`, so a
/// source-tree run always has its checkout as an ancestor of the executable.
/// Accept an ancestor only when it actually looks like this repository, so an
/// installed binary never claims an unrelated directory as a source checkout.
pub(super) fn source_checkout_root_from_exe_path(exe_path: &Path) -> Option<PathBuf> {
    exe_path
        .ancestors()
        .skip(1)
        .find(|ancestor| is_source_checkout_root(ancestor))
        .map(Path::to_path_buf)
}

fn is_source_checkout_root(candidate: &Path) -> bool {
    candidate.join("taarof-app/Cargo.toml").is_file() && candidate.join("taarof-web").is_dir()
}

pub(super) fn repo_web_dist_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("taarof-web/dist")
}

pub(super) fn normalized_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn is_source_checkout_exe(exe_path: &Path, repo_root: &Path) -> bool {
    normalized_path(exe_path).starts_with(normalized_path(repo_root))
}

pub(super) fn installed_web_dist_dir_from_exe_path(exe_path: &Path) -> Option<PathBuf> {
    let bin_dir = exe_path.parent()?;
    let prefix = bin_dir.parent()?;
    Some(prefix.join("share/taarof/web"))
}

pub(super) fn web_asset_candidate_paths_for(
    env_override: Option<PathBuf>,
    current_exe: Option<&Path>,
    data_dir: Option<PathBuf>,
    repo_root: &Path,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env_override {
        push_unique_path(&mut candidates, path);
    }
    let repo_dist_dir = repo_web_dist_dir(repo_root);
    let source_checkout_run = current_exe
        .map(|exe_path| is_source_checkout_exe(exe_path, repo_root))
        .unwrap_or(false);

    if source_checkout_run {
        push_unique_path(&mut candidates, repo_dist_dir.clone());
    }

    if !source_checkout_run {
        if let Some(exe_path) = current_exe {
            if let Some(path) = installed_web_dist_dir_from_exe_path(exe_path) {
                push_unique_path(&mut candidates, path);
            }
        }
    }

    if let Some(data_dir) = data_dir {
        push_unique_path(&mut candidates, data_dir.join("taarof/web"));
    }

    if !source_checkout_run {
        push_unique_path(&mut candidates, repo_dist_dir);
    }

    candidates
}

pub(super) fn web_asset_candidate_paths() -> Vec<PathBuf> {
    let current_exe = std::env::current_exe().ok();
    web_asset_candidate_paths_for(
        std::env::var_os(HTTP_WEB_DIST_ENV).map(PathBuf::from),
        current_exe.as_deref(),
        dirs::data_dir(),
        &repo_root_dir(),
    )
}

pub(super) fn resolve_web_assets(candidates: &[PathBuf]) -> Option<ResolvedWebAssets> {
    for candidate in candidates {
        let index_path = candidate.join("index.html");
        if candidate.is_dir() && index_path.is_file() {
            return Some(ResolvedWebAssets {
                dist_dir: candidate.clone(),
                index_path,
            });
        }
    }

    None
}

pub(super) fn sanitize_web_path(path: &str) -> Option<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(sanitized)
}

pub(super) fn file_response(status: StatusCode, path: &Path) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut response = (status, bytes).into_response();
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref())
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            );
            response
        }
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain; charset=utf-8",
            "failed to read web asset",
        ),
    }
}

pub(super) fn text_response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<String>,
) -> Response {
    let mut response = (status, body.into()).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

// Keep this fallback body pathless so it never echoes local filesystem or
// request paths into the browser-visible error page.
pub(super) const MISSING_WEB_DIST_RESPONSE_BODY: &str = concat!(
    "<!doctype html>",
    "<meta charset=\"utf-8\">",
    "<title>taarof web unavailable</title>",
    "<h1>taarof web bundle missing</h1>",
    "<p>The local web bundle could not be located. See the application diagnostics log for details.</p>",
);
pub(super) const MISSING_WEB_DIST_DIAGNOSTIC_PREFIX: &str =
    "missing web assets; attempted asset paths: ";

pub(super) fn missing_web_dist_diagnostic(attempted_paths: &[PathBuf]) -> String {
    let attempted_paths_text = attempted_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{MISSING_WEB_DIST_DIAGNOSTIC_PREFIX}{attempted_paths_text}")
}

pub(super) fn missing_web_dist_response(attempted_paths: &[PathBuf]) -> Response {
    crate::diagnostics::record_web_asset_lookup_failure(
        missing_web_dist_diagnostic(attempted_paths),
        attempted_paths,
    );
    text_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "text/plain; charset=utf-8",
        MISSING_WEB_DIST_RESPONSE_BODY,
    )
}

pub(super) fn serve_web_request(state: &HttpState, path: &str) -> Response {
    if path == "/api" || path.starts_with("/api/") {
        return text_response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "not found",
        );
    }

    let Some(assets) = resolve_web_assets(&state.web_asset_candidates) else {
        return missing_web_dist_response(&state.web_asset_candidates);
    };

    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return file_response(StatusCode::OK, &assets.index_path);
    }

    let Some(relative) = sanitize_web_path(trimmed) else {
        return text_response(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            "invalid path",
        );
    };

    let candidate = assets.dist_dir.join(relative);
    if confined_file_in_dist(&assets.dist_dir, &candidate) {
        return file_response(StatusCode::OK, &candidate);
    }

    if Path::new(trimmed).extension().is_none() {
        return file_response(StatusCode::OK, &assets.index_path);
    }

    text_response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        "asset not found",
    )
}

/// Confine asset serving to the dist dir.
///
/// `sanitize_web_path` already strips `..` traversal, but `std::fs::read`
/// follows symlinks. A symlink placed inside the dist dir that targets a file
/// outside it would otherwise be served. Canonicalize both the dist dir and the
/// resolved candidate (which resolves every symlink and requires the path to
/// exist) and require the candidate to stay within the canonicalized dist dir
/// before allowing the read.
///
/// A candidate that cannot be canonicalized (e.g. a genuinely missing file)
/// returns `false`, which keeps it on the normal not-found / SPA-fallback path
/// rather than producing an error.
fn confined_file_in_dist(dist_dir: &Path, candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    let Ok(canonical_dist_dir) = std::fs::canonicalize(dist_dir) else {
        return false;
    };
    let Ok(canonical_candidate) = std::fs::canonicalize(candidate) else {
        // Missing or otherwise unresolvable: treat as not found, not an error.
        return false;
    };
    canonical_candidate.starts_with(&canonical_dist_dir)
}
