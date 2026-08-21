/// Git helpers for workspace metadata: repo discovery, branch, dirty state.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

/// A deliberately small, process-wide ceiling for slow Git and worktree
/// operations submitted from GTK.  A per-key single-flight guard alone still
/// permits an unbounded number of distinct paths to fill the worker pool.
pub(crate) const MAX_GIT_ASYNC_IN_FLIGHT: usize = 16;

/// A single-flight request token for filesystem and subprocess Git work.
///
/// GTK owns this gate. The worker receives only immutable request data, and
/// the short GTK apply phase must present this token again before it can mutate
/// application state. Repeating the same key while it is in flight coalesces;
/// invalidating a key makes a late worker result a no-op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitAsyncTicket {
    key: String,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitAsyncSubmission {
    Started,
    Coalesced,
    Saturated,
}

#[derive(Default)]
struct GitAsyncGateState {
    next_generation: u64,
    in_flight: HashMap<String, u64>,
}

/// Main-context-only coordinator for off-thread Git work.
#[derive(Clone, Default)]
pub(crate) struct GitAsyncGate {
    state: Rc<RefCell<GitAsyncGateState>>,
}

thread_local! {
    static GTK_GIT_ASYNC_GATE: GitAsyncGate = GitAsyncGate::default();
}

/// Submit Git work from a GTK callback. The main-context gate is shared by all
/// UI and GTK-dispatched socket callers, so equal request keys coalesce even
/// when they originate from different surfaces.
pub(crate) fn spawn_async<T, Work, Apply>(
    key: impl Into<String>,
    work: Work,
    apply: Apply,
) -> GitAsyncSubmission
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
    Apply: FnOnce(T) + 'static,
{
    GTK_GIT_ASYNC_GATE.with(|gate| gate.spawn(key, work, apply))
}

/// Submit a fallible filesystem/Git worker from GTK and always deliver one
/// completion to the caller.  Unlike [`spawn_async`], a failed worker join is
/// observable by the apply side.  Use this for every caller that owns a
/// subscriber list, an in-flight ledger, or a retryable UI state: silently
/// dropping the join failure leaves those ledgers permanently busy.
pub(crate) fn spawn_async_result<T, Work, Apply>(
    key: impl Into<String>,
    work: Work,
    apply: Apply,
) -> GitAsyncSubmission
where
    T: Send + 'static,
    Work: FnOnce() -> Result<T, String> + Send + 'static,
    Apply: FnOnce(Result<T, String>) + 'static,
{
    GTK_GIT_ASYNC_GATE.with(|gate| gate.spawn_result(key, work, apply))
}

impl GitAsyncGate {
    #[cfg(test)]
    pub(crate) fn begin(&self, key: impl Into<String>) -> Option<GitAsyncTicket> {
        let key = key.into();
        let mut state = self.state.borrow_mut();
        if state.in_flight.contains_key(&key) {
            return None;
        }
        if state.in_flight.len() >= MAX_GIT_ASYNC_IN_FLIGHT {
            return None;
        }
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        state.in_flight.insert(key.clone(), generation);
        Some(GitAsyncTicket { key, generation })
    }

    #[cfg(test)]
    pub(crate) fn invalidate(&self, key: &str) {
        self.state.borrow_mut().in_flight.remove(key);
    }

    #[cfg(test)]
    pub(crate) fn is_current(&self, ticket: &GitAsyncTicket) -> bool {
        self.state.borrow().in_flight.get(&ticket.key) == Some(&ticket.generation)
    }

    fn submission_for(&self, key: impl Into<String>) -> Result<GitAsyncTicket, GitAsyncSubmission> {
        let key = key.into();
        let mut state = self.state.borrow_mut();
        if state.in_flight.contains_key(&key) {
            return Err(GitAsyncSubmission::Coalesced);
        }
        if state.in_flight.len() >= MAX_GIT_ASYNC_IN_FLIGHT {
            return Err(GitAsyncSubmission::Saturated);
        }
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        state.in_flight.insert(key.clone(), generation);
        Ok(GitAsyncTicket { key, generation })
    }

    fn finish(&self, ticket: &GitAsyncTicket) -> bool {
        let mut state = self.state.borrow_mut();
        if state.in_flight.get(&ticket.key) != Some(&ticket.generation) {
            return false;
        }
        state.in_flight.remove(&ticket.key);
        true
    }

    /// Run blocking Git work without blocking the GTK main context.
    pub(crate) fn spawn<T, Work, Apply>(
        &self,
        key: impl Into<String>,
        work: Work,
        apply: Apply,
    ) -> GitAsyncSubmission
    where
        T: Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
        Apply: FnOnce(T) + 'static,
    {
        let ticket = match self.submission_for(key) {
            Ok(ticket) => ticket,
            Err(submission) => return submission,
        };
        self.spawn_ticket(ticket, work, apply);
        GitAsyncSubmission::Started
    }

    /// The fallible counterpart to [`Self::spawn`].  The callback runs after
    /// the ticket is released even when `gio::spawn_blocking` itself failed,
    /// allowing the owner to drain subscribers and clear local in-flight
    /// state.  It is deliberately separate from `spawn`: old callers that
    /// only render best-effort metadata retain their no-apply-on-stale guard.
    pub(crate) fn spawn_result<T, Work, Apply>(
        &self,
        key: impl Into<String>,
        work: Work,
        apply: Apply,
    ) -> GitAsyncSubmission
    where
        T: Send + 'static,
        Work: FnOnce() -> Result<T, String> + Send + 'static,
        Apply: FnOnce(Result<T, String>) + 'static,
    {
        let ticket = match self.submission_for(key) {
            Ok(ticket) => ticket,
            Err(submission) => return submission,
        };
        let gate = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(work)
                .await
                .unwrap_or_else(|_| Err("Git worker did not complete".to_string()));
            // Always release our global ticket before applying. `finish` may
            // report false for a deliberately invalidated generation, but this
            // completion still belongs to its owner and must be allowed to
            // release owner-local ledgers/subscribers safely.
            let _ = gate.finish(&ticket);
            apply(result);
        });
        GitAsyncSubmission::Started
    }

    pub(crate) fn spawn_ticket<T, Work, Apply>(
        &self,
        ticket: GitAsyncTicket,
        work: Work,
        apply: Apply,
    ) where
        T: Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
        Apply: FnOnce(T) + 'static,
    {
        let gate = self.clone();
        glib::spawn_future_local(async move {
            let Ok(result) = gio::spawn_blocking(work).await else {
                let _ = gate.finish(&ticket);
                return;
            };
            if gate.finish(&ticket) {
                apply(result);
            }
        });
    }
}

/// Discovered git metadata for a directory.
#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub repo_root: Option<String>,
    pub checkout_root: Option<String>,
    pub branch: Option<String>,
    pub is_dirty: bool,
    pub upstream: Option<String>,
}

impl GitInfo {
    /// The checkout root the user is actually inside.
    ///
    /// For normal repositories this is the same as `repo_root`. For linked
    /// worktrees it is the linked worktree path rather than the owning/main
    /// checkout.
    pub fn workspace_root(&self) -> Option<&str> {
        self.checkout_root.as_deref().or(self.repo_root.as_deref())
    }

    pub fn is_linked_worktree(&self) -> bool {
        matches!((&self.repo_root, &self.checkout_root), (Some(repo), Some(checkout)) if repo != checkout)
    }

    pub fn working_tree_path(&self) -> Option<&str> {
        self.is_linked_worktree()
            .then_some(self.checkout_root.as_deref())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub is_bare: bool,
}

/// Discover git info for a directory by walking up to find .git.
pub fn discover(cwd: &str) -> GitInfo {
    let Some(paths) = discover_paths(cwd) else {
        return GitInfo::default();
    };

    let branch = read_branch(&paths.checkout_root);
    let is_dirty = check_dirty(Path::new(&paths.checkout_root));
    let upstream = branch
        .as_ref()
        .and_then(|b| read_upstream(&paths.repo_root, &paths.checkout_root, b));

    GitInfo {
        repo_root: Some(paths.repo_root),
        checkout_root: Some(paths.checkout_root),
        branch,
        is_dirty,
        upstream,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoDiscovery {
    repo_root: String,
    checkout_root: String,
}

/// Discover both the owning repository root and the checkout root.
///
/// For normal repositories these are the same path. For linked worktrees,
/// `repo_root` points at the main checkout while `checkout_root` points at the
/// linked worktree path whose HEAD/dirty state should be inspected.
fn discover_paths(cwd: &str) -> Option<RepoDiscovery> {
    let mut dir = absolute_path(Path::new(cwd))?;
    loop {
        let git_entry = dir.join(".git");
        if git_entry.is_dir() {
            let root = normalize_path(&dir.to_string_lossy());
            return Some(RepoDiscovery {
                repo_root: root.clone(),
                checkout_root: root,
            });
        }
        if git_entry.is_file() {
            let checkout_root = normalize_path(&dir.to_string_lossy());
            let repo_root =
                resolve_linked_worktree_repo_root(&dir).unwrap_or_else(|| checkout_root.clone());
            return Some(RepoDiscovery {
                repo_root,
                checkout_root,
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Find the repo root by walking up from cwd looking for .git.
/// In a linked worktree, `.git` is a file pointing to the main repo —
/// we follow that pointer so `repo_root` always refers to the main checkout.
fn find_repo_root(cwd: &str) -> Option<String> {
    discover_paths(cwd).map(|paths| paths.repo_root)
}

fn resolve_linked_worktree_repo_root(checkout_root: &Path) -> Option<String> {
    let gitdir_path = resolve_gitdir_path(checkout_root)?;
    // gitdir is typically <main_repo>/.git/worktrees/<name>
    // The main repo root is the parent of .git
    let dot_git = gitdir_path
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".git"))?;
    let main_root = dot_git.parent()?;
    Some(normalize_path(&main_root.to_string_lossy()))
}

fn resolve_gitdir_path(checkout_root: &Path) -> Option<PathBuf> {
    let git_entry = checkout_root.join(".git");
    if git_entry.is_dir() {
        return Some(git_entry);
    }
    if !git_entry.is_file() {
        return None;
    }

    let contents = std::fs::read_to_string(&git_entry).ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        checkout_root.join(gitdir)
    };
    absolute_path(&path).or(Some(path))
}

fn absolute_path(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    Some(normalize_components(absolute))
}

fn normalize_components(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Resolve a path to a normalized absolute form without touching the
/// filesystem. Git discovery performs canonical reads only on its worker, so
/// this helper stays safe for session serialization and GTK-side request
/// capture.
pub fn normalize_path(path: &str) -> String {
    absolute_path(Path::new(path))
        .unwrap_or_else(|| PathBuf::from(path))
        .to_string_lossy()
        .into_owned()
}

/// Read branch name from the checkout's HEAD file.
fn read_branch(checkout_root: &str) -> Option<String> {
    let head_path = resolve_head_path(Path::new(checkout_root))?;
    let content = std::fs::read_to_string(head_path).ok()?;
    let content = content.trim();
    if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else if content.len() >= 7 {
        Some(content[..7].to_string()) // detached HEAD
    } else {
        Some(content.to_string())
    }
}

fn resolve_head_path(checkout_root: &Path) -> Option<PathBuf> {
    let gitdir = resolve_gitdir_path(checkout_root)?;
    Some(gitdir.join("HEAD"))
}

/// Read the upstream tracking branch (e.g., "origin/main").
fn read_upstream(repo_root: &str, checkout_root: &str, branch: &str) -> Option<String> {
    let path = Path::new(repo_root)
        .join(".git/refs/remotes/origin")
        .join(branch);
    if path.exists() {
        Some(format!("origin/{branch}"))
    } else {
        // Try reading from config.
        // Use the checkout path so linked worktrees resolve branch-specific config
        // in the same context the user is actually on.
        Command::new("git")
            .args(["config", &format!("branch.{branch}.remote")])
            .current_dir(checkout_root)
            .output()
            .ok()
            .and_then(|o| {
                let remote = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if remote.is_empty() {
                    None
                } else {
                    Some(format!("{remote}/{branch}"))
                }
            })
    }
}

/// Format a short display string for the workspace header.
/// e.g., "main" or "main *" (dirty) or "main -> origin/main"
pub fn format_status(info: &GitInfo) -> String {
    let Some(ref branch) = info.branch else {
        return String::new();
    };
    let mut s = branch.clone();
    if info.is_dirty {
        s.push_str(" *");
    }
    if let Some(upstream) = &info.upstream {
        s.push_str(" -> ");
        s.push_str(upstream);
    }
    s
}

/// Extract a workspace name from a repo root path.
/// e.g., "/tmp/user/Documents/taarof" → "taarof"
pub fn workspace_name_from_repo(repo_root: &str) -> String {
    Path::new(repo_root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepositoryIdentities {
    pub head: String,
    pub base: String,
}

pub fn github_repository_identities(checkout_root: &Path) -> Option<GitHubRepositoryIdentities> {
    let gitdir = resolve_gitdir_path(checkout_root)?;
    let dot_git = gitdir
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".git"))?;
    let config = std::fs::read_to_string(dot_git.join("config")).ok()?;
    github_repository_identities_from_config(&config)
}

fn github_repository_identities_from_config(config: &str) -> Option<GitHubRepositoryIdentities> {
    let head = remote_url_from_git_config(config, "origin")
        .and_then(github_repository_slug_from_remote)?;
    let base = remote_url_from_git_config(config, "upstream")
        .and_then(github_repository_slug_from_remote)
        .or_else(|| {
            remote_url_from_git_config(config, "parent")
                .and_then(github_repository_slug_from_remote)
        })
        .unwrap_or_else(|| head.clone());
    Some(GitHubRepositoryIdentities { head, base })
}

fn remote_url_from_git_config<'a>(config: &'a str, remote: &str) -> Option<&'a str> {
    let expected = format!("[remote \"{remote}\"]");
    let mut in_remote = false;
    for raw in config.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_remote = line.eq_ignore_ascii_case(&expected);
            continue;
        }
        if in_remote {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("url") {
                return Some(value.trim());
            }
        }
    }
    None
}

pub(crate) fn github_repository_slug_from_remote(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some()
        || !valid_github_repository_component(owner)
        || !valid_github_repository_component(repo)
    {
        return None;
    }
    Some(format!("{owner}/{repo}").to_ascii_lowercase())
}

fn valid_github_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// List worktrees while preserving command failure for callers that need to
/// distinguish "this repository has no linked worktrees" from "Git could not
/// be queried".  UI caches must never turn the latter into a healthy empty
/// list.
pub fn list_worktrees_checked(repo_root: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output();

    match output {
        Ok(out) if out.status.success() => Ok(parse_worktree_list_porcelain(
            &String::from_utf8_lossy(&out.stdout),
        )),
        Ok(out) => {
            let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::diagnostics::record_command_failure(
                "git",
                "worktree-list",
                format!("git worktree list failed in {}", repo_root.display()),
                Some(serde_json::json!({
                    "repo_root": repo_root.display().to_string(),
                    "status": out.status.to_string(),
                    "stderr": error,
                })),
            );
            Err(format!(
                "git worktree list failed in {}",
                repo_root.display()
            ))
        }
        Err(err) => {
            let error = err.to_string();
            crate::diagnostics::record_command_failure(
                "git",
                "worktree-list",
                format!("failed to run git worktree list in {}", repo_root.display()),
                Some(serde_json::json!({
                    "repo_root": repo_root.display().to_string(),
                    "error": error,
                })),
            );
            Err(format!(
                "failed to run git worktree list in {}",
                repo_root.display()
            ))
        }
    }
}

/// Compatibility helper for callers that deliberately treat an unavailable
/// listing as empty (mainly legacy/headless presentation code).  New UI work
/// must use [`list_worktrees_checked`] and surface the failure.
#[cfg(test)]
pub fn list_worktrees(repo_root: &Path) -> Vec<WorktreeInfo> {
    list_worktrees_checked(repo_root).unwrap_or_default()
}

pub fn create_worktree(repo_root: &Path, branch_name: &str) -> Result<String, String> {
    let branch_name = branch_name.trim();
    if branch_name.is_empty() {
        return Err("branch name cannot be empty".into());
    }

    let top_level = resolve_git_toplevel(repo_root)?;
    if check_dirty(&top_level) {
        eprintln!(
            "taarof: creating worktree from dirty repository {}",
            top_level.display()
        );
    }

    let worktree_path = build_worktree_path(&top_level, branch_name);
    let existed = branch_exists(&top_level, branch_name);
    let mut command = Command::new("git");
    command.args(["worktree", "add"]);
    if !existed {
        command.args(["-b", branch_name]);
    }
    command.arg(&worktree_path);
    if existed {
        command.arg(branch_name);
    }

    let output = command.current_dir(&top_level).output().map_err(|err| {
        crate::diagnostics::record_command_failure(
            "git",
            "worktree-add",
            format!("failed to run git worktree add in {}", top_level.display()),
            Some(serde_json::json!({
                "repo_root": top_level.display().to_string(),
                "branch_name": branch_name,
                "worktree_path": worktree_path.display().to_string(),
                "error": err.to_string(),
            })),
        );
        format!(
            "failed to run git worktree add in {}: {err}",
            top_level.display()
        )
    })?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        crate::diagnostics::record_command_failure(
            "git",
            "worktree-add",
            format!("git worktree add failed in {}", top_level.display()),
            Some(serde_json::json!({
                "repo_root": top_level.display().to_string(),
                "branch_name": branch_name,
                "worktree_path": worktree_path.display().to_string(),
                "status": output.status.to_string(),
                "stderr": error,
            })),
        );
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(worktree_path.to_string_lossy().into_owned())
}

#[cfg(test)]
pub fn remove_worktree(worktree_path: &Path) -> Result<(), String> {
    remove_worktree_from_repo(None, worktree_path)
}

pub fn remove_worktree_from_repo(
    repo_root_hint: Option<&Path>,
    worktree_path: &Path,
) -> Result<(), String> {
    let repo_root = repo_root_hint
        .map(|hint| {
            find_repo_root(&hint.to_string_lossy())
                .unwrap_or_else(|| normalize_path(&hint.to_string_lossy()))
        })
        .or_else(|| find_repo_root(&worktree_path.to_string_lossy()))
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "failed to resolve owning repository for {}",
                worktree_path.display()
            )
        })?;

    let output = Command::new("git")
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .current_dir(&repo_root)
        .output()
        .map_err(|err| {
            crate::diagnostics::record_command_failure(
                "git",
                "worktree-remove",
                format!(
                    "failed to run git worktree remove from {}",
                    repo_root.display()
                ),
                Some(serde_json::json!({
                    "repo_root": repo_root.display().to_string(),
                    "worktree_path": worktree_path.display().to_string(),
                    "error": err.to_string(),
                })),
            );
            format!(
                "failed to run git worktree remove from {}: {err}",
                repo_root.display()
            )
        })?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        crate::diagnostics::record_command_failure(
            "git",
            "worktree-remove",
            format!("git worktree remove failed from {}", repo_root.display()),
            Some(serde_json::json!({
                "repo_root": repo_root.display().to_string(),
                "worktree_path": worktree_path.display().to_string(),
                "status": output.status.to_string(),
                "stderr": error,
            })),
        );
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    Ok(())
}

fn resolve_git_toplevel(repo_root: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repo_root)
        .output()
        .map_err(|err| {
            format!(
                "failed to run git rev-parse in {}: {err}",
                repo_root.display()
            )
        })?;

    if !output.status.success() {
        return Err(format!("{} is not a git repository", repo_root.display()));
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        Err(format!(
            "git did not return a top-level path for {}",
            repo_root.display()
        ))
    } else {
        Ok(PathBuf::from(root))
    }
}

fn branch_exists(repo_root: &Path, branch_name: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch_name}"),
        ])
        .current_dir(repo_root)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn build_worktree_path(repo_root: &Path, branch_name: &str) -> PathBuf {
    let repo_parent = repo_root.parent().unwrap_or(repo_root);
    let repo_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".into());
    repo_parent.join(format!(
        "{repo_name}-{}",
        sanitize_worktree_branch(branch_name)
    ))
}

fn sanitize_worktree_branch(branch_name: &str) -> String {
    branch_name
        .trim()
        .replace('/', "-")
        .replace("\\", "-")
        .replace(' ', "-")
        .replace("..", "-")
        .replace(
            ['$', ';', '&', '|', '`', '\'', '"', '*', '?', '~', '^', ':'],
            "-",
        )
}

fn parse_worktree_list_porcelain(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current = WorktreeInfo {
        path: String::new(),
        branch: None,
        is_bare: false,
    };

    for line in output.lines() {
        if line.is_empty() {
            if !current.path.is_empty() {
                worktrees.push(current);
                current = WorktreeInfo {
                    path: String::new(),
                    branch: None,
                    is_bare: false,
                };
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current.path = path.to_string();
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            current.branch = Some(branch.to_string());
        } else if line == "bare" {
            current.is_bare = true;
        }
    }

    if !current.path.is_empty() {
        worktrees.push(current);
    }

    worktrees
}

fn check_dirty(repo_root: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain", "-uno"])
        .current_dir(repo_root)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "taarof-worktree-test-{}-{name}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            run_git(
                root.parent().unwrap(),
                &["init", "-b", "main", root.to_str().unwrap()],
            );
            run_git(&root, &["config", "user.email", "taarof@example.com"]);
            run_git(&root, &["config", "user.name", "taarof"]);
            std::fs::write(root.join("README.md"), "hello\n").unwrap();
            run_git(&root, &["add", "README.md"]);
            run_git(&root, &["commit", "-m", "init"]);
            Self { root }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn list_worktrees_returns_main_worktree_for_repo_without_linked_worktrees() {
        let repo = TempRepo::new("list-main");
        let worktrees = list_worktrees(&repo.root);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, repo.root.to_string_lossy());
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(!worktrees[0].is_bare);
    }

    #[test]
    fn format_status_includes_dirty_marker_and_upstream() {
        let info = GitInfo {
            repo_root: Some("/repo".into()),
            checkout_root: Some("/repo".into()),
            branch: Some("main".into()),
            is_dirty: true,
            upstream: Some("origin/main".into()),
        };

        assert_eq!(format_status(&info), "main * -> origin/main");
    }

    #[test]
    fn format_status_returns_empty_string_without_branch() {
        assert!(format_status(&GitInfo::default()).is_empty());
    }

    #[test]
    fn find_repo_root_from_dot_returns_absolute_path() {
        let repo_root = find_repo_root(".").expect("current package should be inside a git repo");
        assert!(Path::new(&repo_root).is_absolute());
        assert!(Path::new(&repo_root).join(".git").exists());
    }

    #[test]
    fn create_worktree_creates_new_branch_path() {
        let repo = TempRepo::new("create-new");
        let worktree_path = create_worktree(&repo.root, "feature/login").unwrap();
        assert!(Path::new(&worktree_path).exists());
        assert_eq!(
            PathBuf::from(&worktree_path),
            build_worktree_path(&repo.root, "feature/login")
        );
    }

    #[test]
    fn discover_uses_linked_worktree_head_instead_of_main_checkout() {
        let repo = TempRepo::new("discover-linked-branch");
        let worktree_path = create_worktree(&repo.root, "feature/worktree-branch").unwrap();
        let nested = Path::new(&worktree_path).join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let info = discover(nested.to_str().unwrap());

        assert_eq!(
            info.repo_root.as_deref(),
            Some(repo.root.to_string_lossy().as_ref())
        );
        assert_eq!(info.checkout_root.as_deref(), Some(worktree_path.as_str()));
        assert!(info.is_linked_worktree());
        assert_eq!(info.working_tree_path(), Some(worktree_path.as_str()));
        assert_eq!(info.workspace_root(), Some(worktree_path.as_str()));
        assert_eq!(info.branch.as_deref(), Some("feature/worktree-branch"));
        assert!(!info.is_dirty);
    }

    #[test]
    fn create_worktree_uses_existing_branch_without_b_flag() {
        let repo = TempRepo::new("create-existing");
        run_git(&repo.root, &["branch", "feature/existing"]);
        let worktree_path = create_worktree(&repo.root, "feature/existing").unwrap();
        assert!(Path::new(&worktree_path).exists());
    }

    #[test]
    fn list_worktrees_reports_all_created_worktrees() {
        let repo = TempRepo::new("list-many");
        create_worktree(&repo.root, "feature/a").unwrap();
        create_worktree(&repo.root, "feature/b").unwrap();
        let worktrees = list_worktrees(&repo.root);
        assert_eq!(worktrees.len(), 3);
    }

    #[test]
    fn remove_worktree_deletes_directory() {
        let repo = TempRepo::new("remove");
        let worktree_path = create_worktree(&repo.root, "feature/remove").unwrap();
        remove_worktree(Path::new(&worktree_path)).unwrap();
        assert!(!Path::new(&worktree_path).exists());
    }

    #[test]
    fn remove_worktree_handles_missing_directory_with_repo_root_hint() {
        let repo = TempRepo::new("remove-missing");
        let worktree_path = create_worktree(&repo.root, "feature/missing").unwrap();
        std::fs::remove_dir_all(&worktree_path).unwrap();

        remove_worktree_from_repo(Some(&repo.root), Path::new(&worktree_path)).unwrap();

        let worktrees = list_worktrees(&repo.root);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, repo.root.to_string_lossy());
    }

    #[test]
    fn create_worktree_fails_for_non_git_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "taarof-worktree-test-non-git-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let err = create_worktree(&dir, "feature/nope").unwrap_err();
        assert!(err.contains("not a git repository"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_branch_names_for_worktree_paths() {
        assert_eq!(sanitize_worktree_branch("feature/login"), "feature-login");
        assert_eq!(sanitize_worktree_branch("already-clean"), "already-clean");
        assert_eq!(sanitize_worktree_branch("../evil"), "--evil");
        assert_eq!(sanitize_worktree_branch("feature login"), "feature-login");
        assert_eq!(sanitize_worktree_branch("test$;branch"), "test--branch");
        assert_eq!(sanitize_worktree_branch("  spaces  "), "spaces");
    }

    #[test]
    fn parses_porcelain_worktree_output() {
        let output = "\
worktree /tmp/repo\n\
HEAD 1234567\n\
branch refs/heads/main\n\
\n\
worktree /tmp/repo-feature\n\
HEAD 89abcde\n\
branch refs/heads/feature/login\n\
bare\n\
";
        let worktrees = parse_worktree_list_porcelain(output);
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].path, "/tmp/repo");
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(!worktrees[0].is_bare);
        assert_eq!(worktrees[1].branch.as_deref(), Some("feature/login"));
        assert!(worktrees[1].is_bare);
    }

    #[test]
    fn github_remote_slug_normalizes_supported_remote_forms() {
        for remote in [
            "https://github.com/Example-Org/Example-Repo.git",
            "git@github.com:Example-Org/Example-Repo.git",
            "ssh://git@github.com/Example-Org/Example-Repo",
        ] {
            assert_eq!(
                github_repository_slug_from_remote(remote).as_deref(),
                Some("example-org/example-repo")
            );
        }
        assert!(github_repository_slug_from_remote("https://gitlab.com/owner/repo").is_none());
        assert!(
            github_repository_slug_from_remote("https://github.com/owner/repo/extra").is_none()
        );
        assert_eq!(
            remote_url_from_git_config(
                "[core]\n bare = false\n[remote \"origin\"]\n url = git@github.com:Owner/Repo.git\n"
                , "origin"
            ),
            Some("git@github.com:Owner/Repo.git")
        );
    }

    #[test]
    fn github_repository_identities_distinguish_fork_origin_and_upstream_base() {
        let config = "[remote \"origin\"]\n url = git@github.com:forker/legacy-app.git\n[remote \"upstream\"]\n url = https://github.com/Example-Org/Example-Repo.git\n";
        assert_eq!(
            github_repository_identities_from_config(config),
            Some(GitHubRepositoryIdentities {
                head: "forker/legacy-app".into(),
                base: "example-org/example-repo".into(),
            })
        );

        let parent = "[remote \"origin\"]\n url = https://github.com/forker/legacy-app.git\n[remote \"parent\"]\n url = git@github.com:Example-Org/Example-Repo.git\n";
        assert_eq!(
            github_repository_identities_from_config(parent),
            Some(GitHubRepositoryIdentities {
                head: "forker/legacy-app".into(),
                base: "example-org/example-repo".into(),
            })
        );
    }

    #[test]
    fn test_git_async_delayed_git_does_not_block_main_context() {
        let _glib_guard = crate::glib_main_context_test_guard();
        let context = glib::MainContext::default();
        let _acquire = context.acquire().expect("test owns the main context");
        let gate = GitAsyncGate::default();
        let heartbeat = std::rc::Rc::new(std::cell::Cell::new(0));
        let socket_query = std::rc::Rc::new(std::cell::Cell::new(false));
        let applied = std::rc::Rc::new(std::cell::Cell::new(false));

        let heartbeat_for_idle = heartbeat.clone();
        glib::idle_add_local_once(move || heartbeat_for_idle.set(heartbeat_for_idle.get() + 1));
        let socket_query_for_idle = socket_query.clone();
        glib::idle_add_local_once(move || socket_query_for_idle.set(true));

        let applied_for_worker = applied.clone();
        assert!(matches!(
            gate.spawn(
                "delayed-discovery",
                || {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    Ok::<_, String>(())
                },
                move |_| applied_for_worker.set(true)
            ),
            GitAsyncSubmission::Started
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(40);
        while std::time::Instant::now() < deadline && (!socket_query.get() || heartbeat.get() == 0)
        {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert!(
            socket_query.get(),
            "another socket query should run while Git is delayed"
        );
        assert!(
            heartbeat.get() > 0,
            "GLib heartbeat should run while Git is delayed"
        );
        assert!(
            !applied.get(),
            "slow Git result must not apply before it finishes"
        );

        // Drain the worker before returning: GLib sources are process-global
        // in tests, so leaving this future pending could make a later test
        // finalize GTK-owned state on a different test thread.
        let completion_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < completion_deadline && !applied.get() {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(applied.get(), "delayed Git work should eventually apply");
    }

    #[test]
    fn test_git_async_stale_generation_result_is_not_applied() {
        let _glib_guard = crate::glib_main_context_test_guard();
        // A stale ticket never applies, so this test has no signal for the
        // spawned future finishing.  Own a private context instead of the
        // process-global one: a source still pending here is then dropped with
        // this context on this thread rather than being finalized by whichever
        // test thread iterates the default context next.
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let _acquire = context.acquire().expect("test owns the main context");
                let gate = GitAsyncGate::default();
                let (worker_done_tx, worker_done_rx) = std::sync::mpsc::channel();
                let (applied_tx, applied_rx) = std::sync::mpsc::channel();

                let ticket = gate.begin("workspace:7").expect("first request starts");
                gate.spawn_ticket(
                    ticket.clone(),
                    move || {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        worker_done_tx
                            .send(())
                            .expect("worker completion should send");
                        Ok::<_, String>(())
                    },
                    move |_| applied_tx.send(()).expect("stale apply must not run"),
                );
                gate.invalidate("workspace:7");

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while std::time::Instant::now() < deadline && worker_done_rx.try_recv().is_err() {
                    context.iteration(false);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                for _ in 0..10 {
                    context.iteration(false);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                // A stale ticket must neither mutate UI state nor leave the new generation busy.
                assert!(applied_rx.try_recv().is_err());
                assert!(!gate.is_current(&ticket));
            })
            .expect("test owns the private main context");
    }

    #[test]
    fn test_git_async_duplicate_submissions_coalesce() {
        let gate = GitAsyncGate::default();
        assert!(gate.begin("worktree:/repo:feature").is_some());
        assert!(gate.begin("worktree:/repo:feature").is_none());
    }

    #[test]
    fn test_git_async_distinct_keys_are_globally_bounded() {
        let gate = GitAsyncGate::default();
        for index in 0..MAX_GIT_ASYNC_IN_FLIGHT {
            assert!(
                gate.begin(format!("distinct-worktree:{index}")).is_some(),
                "key {index} should fit below the global worker ceiling"
            );
        }
        assert!(
            gate.begin("distinct-worktree:overflow").is_none(),
            "a new distinct key must not bypass the global worker ceiling"
        );
    }

    #[test]
    fn test_git_async_real_worktree_success_partial_failure_and_rollback_are_observable() {
        let repo = TempRepo::new("async-outcomes");
        let worktree_path = create_worktree(&repo.root, "feature/async-outcomes")
            .expect("successful worktree creation should be observable");
        assert!(Path::new(&worktree_path).is_dir());

        let missing = repo.root.join("not-a-worktree");
        let partial_failure = remove_worktree_from_repo(Some(&repo.root), &missing)
            .expect_err("a missing worktree removal must remain observable");
        assert!(partial_failure.contains("failed") || partial_failure.contains("not"));

        remove_worktree_from_repo(Some(&repo.root), Path::new(&worktree_path))
            .expect("rollback should remove the worktree created before the partial failure");
        assert!(!Path::new(&worktree_path).exists());
        assert_eq!(list_worktrees(&repo.root).len(), 1);
    }
}
