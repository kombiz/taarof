use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const PROJECTS_SCHEMA: &str = "taarof.projects.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProjectHost {
    Local,
    Ssh {
        name: String,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection_argv: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectOpenMode {
    Regular,
    Tmux,
    RemoteTmux,
    Coder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoderProject {
    pub workspace_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub repo_path: String,
    #[serde(default)]
    pub create_if_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredProject {
    pub id: String,
    pub display_name: String,
    pub canonical_repo: String,
    pub checkout_root: String,
    pub host: ProjectHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_branch: Option<String>,
    pub preferred_open: ProjectOpenMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coder: Option<CoderProject>,
    pub created_at_unix_ms: u64,
    pub last_opened_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRegistry {
    #[serde(default = "projects_schema")]
    pub schema: String,
    #[serde(default)]
    pub projects: Vec<RegisteredProject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistrySignature {
    modified: Option<SystemTime>,
    len: u64,
}

#[derive(Debug, Clone)]
struct RegistryCacheEntry {
    path: PathBuf,
    signature: Option<RegistrySignature>,
    registry: ProjectRegistry,
}

fn registry_cache() -> &'static Mutex<Option<RegistryCacheEntry>> {
    static CACHE: OnceLock<Mutex<Option<RegistryCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn registry_io_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn lock_registry_file() -> io::Result<std::fs::File> {
    let path = projects_path().with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;
    }
    let file = open_private_lock_file(&path)?;
    file.lock_exclusive()?;
    Ok(file)
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            schema: PROJECTS_SCHEMA.to_string(),
            projects: Vec::new(),
        }
    }
}

impl ProjectRegistry {
    pub fn upsert(&mut self, mut project: RegisteredProject) {
        if let Some(existing) = self.projects.iter_mut().find(|item| item.id == project.id) {
            project.created_at_unix_ms = existing.created_at_unix_ms;
            project.last_opened_at_unix_ms = existing.last_opened_at_unix_ms;
            if project.coder.is_none() {
                project.coder = existing.coder.clone();
            }
            if existing.preferred_open == ProjectOpenMode::Coder && project.coder.is_some() {
                project.preferred_open = ProjectOpenMode::Coder;
            }
            *existing = project;
        } else {
            self.projects.push(project);
        }
        self.projects.sort_by(|left, right| {
            left.display_name
                .to_ascii_lowercase()
                .cmp(&right.display_name.to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|project| project.id != id);
        self.projects.len() != before
    }
}

fn projects_schema() -> String {
    PROJECTS_SCHEMA.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusedProjectSource {
    Local {
        cwd: String,
        preferred_open: ProjectOpenMode,
    },
    Remote {
        cwd: String,
        host_name: String,
        ssh_target: String,
        ssh_argv: Option<Vec<String>>,
        preferred_open: ProjectOpenMode,
    },
}

fn classify_focused_context(
    cwd: String,
    cwd_host: Option<String>,
    ssh_argv: Option<Vec<String>>,
    tmux_target: Option<crate::tmux::TmuxTarget>,
) -> Result<FocusedProjectSource, String> {
    if let Some(target) = tmux_target {
        return match target {
            crate::tmux::TmuxTarget::Local => Ok(FocusedProjectSource::Local {
                cwd,
                preferred_open: ProjectOpenMode::Tmux,
            }),
            crate::tmux::TmuxTarget::Remote { ssh_target } => Ok(FocusedProjectSource::Remote {
                cwd,
                host_name: cwd_host.unwrap_or_else(|| ssh_target.clone()),
                ssh_target,
                ssh_argv,
                preferred_open: ProjectOpenMode::RemoteTmux,
            }),
        };
    }
    if cwd_host.is_some() || ssh_argv.is_some() {
        let host_name =
            cwd_host.ok_or_else(|| "remote pane has not reported its host identity".to_string())?;
        let ssh_argv =
            ssh_argv.ok_or_else(|| "remote pane has no reusable SSH command".to_string())?;
        let ssh_target = crate::terminal::registered_project_ssh_target(&ssh_argv)
            .ok_or_else(|| "remote pane SSH target is unavailable".to_string())?;
        return Ok(FocusedProjectSource::Remote {
            cwd,
            host_name,
            ssh_target,
            ssh_argv: Some(ssh_argv),
            preferred_open: ProjectOpenMode::Regular,
        });
    }
    Ok(FocusedProjectSource::Local {
        cwd,
        preferred_open: ProjectOpenMode::Regular,
    })
}

pub fn focused_project_source(state: &crate::AppState) -> Result<FocusedProjectSource, String> {
    let tab = state
        .active_ws()
        .and_then(|workspace| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.id == workspace.active_tab)
        })
        .ok_or_else(|| "no focused terminal tab".to_string())?;
    let pane = tab
        .panes
        .leaf(tab.focused_pane_id)
        .ok_or_else(|| "focused pane is not available".to_string())?;
    let cwd = pane
        .tmux_backing
        .as_ref()
        .and_then(|backing| backing.pane_info.value.as_ref())
        .map(|info| info.cwd.clone())
        .or_else(|| pane.location_state.cwd.clone())
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| "focused pane has not reported a working directory".to_string())?;
    classify_focused_context(
        cwd,
        pane.location_state.cwd_host.clone(),
        pane.process_state.ssh_command.clone(),
        pane.tmux_backing
            .as_ref()
            .map(|backing| backing.target.clone()),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectOpenPlan {
    Local {
        working_dir: String,
        tmux: bool,
    },
    Remote {
        host_name: String,
        ssh_target: String,
        connection_argv: Option<Vec<String>>,
        working_dir: String,
        tmux: bool,
    },
    Coder {
        list_command: Vec<String>,
        create_if_missing: bool,
        create_command: Option<Vec<String>>,
        connect_command: Vec<String>,
    },
}

pub fn stable_project_id(canonical_repo: &str, host: &ProjectHost, checkout_root: &str) -> String {
    let host_key = match host {
        ProjectHost::Local => "local".to_string(),
        ProjectHost::Ssh { target, .. } => format!("ssh:{target}"),
    };
    let mut hasher = Sha256::new();
    hasher.update(canonical_repo.to_ascii_lowercase());
    hasher.update(b"\0");
    hasher.update(host_key);
    hasher.update(b"\0");
    hasher.update(checkout_root);
    let digest = hasher.finalize();
    let hash = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("project-{hash}")
}

pub fn project_from_local_checkout(
    cwd: &str,
    preferred_open: ProjectOpenMode,
) -> Result<RegisteredProject, String> {
    let info = crate::git::discover(cwd);
    let checkout_root = info
        .workspace_root()
        .ok_or_else(|| "focused pane is not inside a Git repository".to_string())?;
    let identities = crate::git::github_repository_identities(Path::new(checkout_root))
        .ok_or_else(|| "could not determine a canonical GitHub repository".to_string())?;
    build_project(
        identities.base,
        checkout_root.to_string(),
        info.branch,
        ProjectHost::Local,
        preferred_open,
    )
}

pub fn project_from_remote_identity(
    host_name: &str,
    ssh_target: &str,
    ssh_argv: Option<&[String]>,
    identity: crate::tracking::RemoteGitIdentity,
    preferred_open: ProjectOpenMode,
) -> Result<RegisteredProject, String> {
    validate_ssh_identity(host_name, ssh_target)?;
    build_project(
        identity.base_repository,
        identity.checkout_root,
        Some(identity.branch),
        ProjectHost::Ssh {
            name: host_name.to_string(),
            target: ssh_target.to_string(),
            connection_argv: sanitized_ssh_connection(ssh_argv, ssh_target),
        },
        preferred_open,
    )
}

fn build_project(
    canonical_repo: String,
    checkout_root: String,
    branch: Option<String>,
    host: ProjectHost,
    preferred_open: ProjectOpenMode,
) -> Result<RegisteredProject, String> {
    validate_project_field("repository", &canonical_repo)?;
    validate_project_field("checkout root", &checkout_root)?;
    if let Some(branch) = branch.as_deref() {
        validate_project_field("branch", branch)?;
    }
    let display_name = canonical_repo
        .rsplit_once('/')
        .map_or(canonical_repo.as_str(), |(_, name)| name)
        .to_string();
    let now = now_unix_ms();
    Ok(RegisteredProject {
        id: stable_project_id(&canonical_repo, &host, &checkout_root),
        display_name,
        canonical_repo,
        checkout_root,
        host,
        last_branch: branch,
        preferred_open,
        coder: None,
        created_at_unix_ms: now,
        last_opened_at_unix_ms: now,
    })
}

fn validate_project_field(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(format!(
            "project {label} is empty or contains control characters"
        ));
    }
    Ok(())
}

fn validate_cli_identifier(label: &str, value: &str) -> Result<(), String> {
    validate_project_field(label, value)?;
    if value.starts_with('-')
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ".-_@".contains(ch))
    {
        return Err(format!("project {label} contains unsupported characters"));
    }
    Ok(())
}

pub(crate) fn validate_ssh_identity(name: &str, target: &str) -> Result<(), String> {
    validate_project_field("host name", name)?;
    validate_project_field("SSH target", target)?;
    if target.starts_with('-')
        || !target
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ".-_@:%[]".contains(ch))
    {
        return Err("project SSH target contains unsupported characters".to_string());
    }
    Ok(())
}

pub fn build_open_plan(project: &RegisteredProject) -> Result<ProjectOpenPlan, String> {
    match (&project.preferred_open, &project.host) {
        (ProjectOpenMode::Coder, _) => build_coder_open_plan(project),
        (ProjectOpenMode::Regular, ProjectHost::Local) => Ok(ProjectOpenPlan::Local {
            working_dir: project.checkout_root.clone(),
            tmux: false,
        }),
        (ProjectOpenMode::Tmux, ProjectHost::Local) => Ok(ProjectOpenPlan::Local {
            working_dir: project.checkout_root.clone(),
            tmux: true,
        }),
        (
            ProjectOpenMode::Regular,
            ProjectHost::Ssh {
                name,
                target,
                connection_argv,
            },
        ) => {
            validate_ssh_identity(name, target)?;
            Ok(ProjectOpenPlan::Remote {
                host_name: name.clone(),
                ssh_target: target.clone(),
                connection_argv: connection_argv.clone(),
                working_dir: project.checkout_root.clone(),
                tmux: false,
            })
        }
        (
            ProjectOpenMode::RemoteTmux,
            ProjectHost::Ssh {
                name,
                target,
                connection_argv,
            },
        ) => {
            validate_ssh_identity(name, target)?;
            Ok(ProjectOpenPlan::Remote {
                host_name: name.clone(),
                ssh_target: target.clone(),
                connection_argv: connection_argv.clone(),
                working_dir: project.checkout_root.clone(),
                tmux: true,
            })
        }
        (ProjectOpenMode::RemoteTmux, ProjectHost::Local) => {
            Err("remote-tmux projects require an SSH host".to_string())
        }
        (ProjectOpenMode::Tmux, ProjectHost::Ssh { .. }) => {
            Err("SSH projects must use remote-tmux instead of tmux".to_string())
        }
    }
}

fn build_coder_open_plan(project: &RegisteredProject) -> Result<ProjectOpenPlan, String> {
    let coder = project
        .coder
        .as_ref()
        .ok_or_else(|| "Coder open mode requires a Coder project binding".to_string())?;
    validate_cli_identifier("Coder workspace", &coder.workspace_name)?;
    validate_project_field("Coder repository path", &coder.repo_path)?;
    if let Some(template) = coder.template.as_deref() {
        validate_cli_identifier("Coder template", template)?;
    }
    let create_command = if coder.create_if_missing {
        coder.template.as_ref().map(|template| {
            vec![
                "coder".to_string(),
                "create".to_string(),
                coder.workspace_name.clone(),
                "--template".to_string(),
                template.clone(),
                "--yes".to_string(),
            ]
        })
    } else {
        None
    };
    let remote_command = format!(
        "cd -- {}; exec ${{SHELL:-/bin/sh}} -l",
        shell_quote(&coder.repo_path)
    );
    Ok(ProjectOpenPlan::Coder {
        list_command: vec![
            "coder".to_string(),
            "list".to_string(),
            "--output".to_string(),
            "json".to_string(),
        ],
        create_if_missing: coder.create_if_missing,
        create_command,
        connect_command: vec![
            "coder".to_string(),
            "ssh".to_string(),
            coder.workspace_name.clone(),
            "--".to_string(),
            "sh".to_string(),
            "-lc".to_string(),
            remote_command,
        ],
    })
}

fn sanitized_ssh_connection(ssh_argv: Option<&[String]>, target: &str) -> Option<Vec<String>> {
    let argv = ssh_argv?;
    let target_index = crate::terminal::ssh_command_target_index(argv)?;
    if argv.get(target_index)? != target {
        return None;
    }
    let mut safe = vec!["ssh".to_string()];
    let mut index = 1;
    while index < target_index {
        let argument = argv[index].as_str();
        if matches!(argument, "-p" | "-J" | "-l") {
            let value = argv.get(index + 1)?;
            if value.starts_with('-') || value.chars().any(char::is_control) {
                return None;
            }
            safe.push(argument.to_string());
            safe.push(value.clone());
            index += 2;
            continue;
        }
        let safe_attached = ["-p", "-J", "-l"].iter().any(|prefix| {
            argument
                .strip_prefix(prefix)
                .is_some_and(|value| !value.is_empty() && !value.chars().any(char::is_control))
        });
        if safe_attached || matches!(argument, "-4" | "-6" | "-C" | "-q") {
            safe.push(argument.to_string());
        }
        index += 1;
    }
    safe.push(target.to_string());
    Some(safe)
}

pub fn remote_shell_argv(
    ssh_target: &str,
    connection_argv: Option<&[String]>,
    working_dir: &str,
) -> Result<Vec<String>, String> {
    validate_ssh_identity(ssh_target, ssh_target)?;
    validate_project_field("checkout root", working_dir)?;
    let mut argv = sanitized_ssh_connection(connection_argv, ssh_target)
        .unwrap_or_else(|| vec!["ssh".to_string(), ssh_target.to_string()]);
    argv.insert(argv.len() - 1, "-t".to_string());
    argv.push(format!(
        "cd -- {}; exec ${{SHELL:-/bin/sh}} -l",
        shell_quote(working_dir)
    ));
    Ok(argv)
}

pub fn run_guarded_coder_create(command: &[String]) -> Result<(), String> {
    if command.first().map(String::as_str) != Some("coder")
        || command.get(1).map(String::as_str) != Some("create")
        || !command.iter().any(|argument| argument == "--yes")
    {
        return Err("refusing unguarded Coder creation command".to_string());
    }
    let output = std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("could not run coder create: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            Err("coder create failed".to_string())
        } else {
            let tail = detail
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            Err(format!("coder create failed: {tail}"))
        }
    }
}

pub fn coder_workspace_exists(output: &str, workspace_name: &str) -> Result<bool, String> {
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|_| "coder list returned invalid JSON".to_string())?;
    let entries = value
        .as_array()
        .or_else(|| {
            value
                .get("workspaces")
                .and_then(serde_json::Value::as_array)
        })
        .ok_or_else(|| "coder list returned an unsupported JSON shape".to_string())?;
    Ok(entries.iter().any(|entry| {
        entry.get("name").and_then(serde_json::Value::as_str) == Some(workspace_name)
            || entry
                .get("latest_build")
                .and_then(|build| build.get("workspace_name"))
                .and_then(serde_json::Value::as_str)
                == Some(workspace_name)
    }))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn list() -> io::Result<Vec<RegisteredProject>> {
    Ok(cached_registry(&projects_path())?.projects)
}

pub fn save(project: RegisteredProject) -> io::Result<()> {
    let _guard = registry_io_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_lock = lock_registry_file()?;
    validate_registered_project(&project)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let path = projects_path();
    let mut registry = load_registry(&path)?;
    registry.upsert(project);
    save_registry(&path, &registry)
}

pub fn mark_opened(id: &str) -> io::Result<()> {
    let _guard = registry_io_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_lock = lock_registry_file()?;
    let path = projects_path();
    let mut registry = load_registry(&path)?;
    let project = registry
        .projects
        .iter_mut()
        .find(|project| project.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "project not found"))?;
    project.last_opened_at_unix_ms = now_unix_ms();
    save_registry(&path, &registry)
}

pub fn forget(id: &str) -> io::Result<bool> {
    let _guard = registry_io_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_lock = lock_registry_file()?;
    let path = projects_path();
    let mut registry = load_registry(&path)?;
    if !registry.remove(id) {
        return Ok(false);
    }
    save_registry(&path, &registry)?;
    Ok(true)
}

fn projects_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        })
        .join("taarof/projects.json")
}

fn cached_registry(path: &Path) -> io::Result<ProjectRegistry> {
    let signature = registry_signature(path)?;
    if let Some(entry) = registry_cache()
        .lock()
        .expect("project registry cache lock should not be poisoned")
        .as_ref()
        .filter(|entry| entry.path == path && entry.signature == signature)
    {
        return Ok(entry.registry.clone());
    }
    let registry = load_registry(path)?;
    cache_registry(path, &registry);
    Ok(registry)
}

fn registry_signature(path: &Path) -> io::Result<Option<RegistrySignature>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(RegistrySignature {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn cache_registry(path: &Path, registry: &ProjectRegistry) {
    let signature = registry_signature(path).ok().flatten();
    *registry_cache()
        .lock()
        .expect("project registry cache lock should not be poisoned") = Some(RegistryCacheEntry {
        path: path.to_path_buf(),
        signature,
        registry: registry.clone(),
    });
}

fn validate_registered_project(project: &RegisteredProject) -> Result<(), String> {
    validate_project_field("display name", &project.display_name)?;
    validate_project_field("repository", &project.canonical_repo)?;
    validate_project_field("checkout root", &project.checkout_root)?;
    if let Some(branch) = project.last_branch.as_deref() {
        validate_project_field("branch", branch)?;
    }
    if let ProjectHost::Ssh {
        name,
        target,
        connection_argv,
    } = &project.host
    {
        validate_ssh_identity(name, target)?;
        if let Some(argv) = connection_argv {
            if sanitized_ssh_connection(Some(argv), target).as_ref() != Some(argv) {
                return Err("SSH connection command contains unsupported options".to_string());
            }
        }
    }
    if let Some(coder) = project.coder.as_ref() {
        validate_cli_identifier("Coder workspace", &coder.workspace_name)?;
        validate_project_field("Coder repository path", &coder.repo_path)?;
        if let Some(template) = coder.template.as_deref() {
            validate_cli_identifier("Coder template", template)?;
        }
    }
    Ok(())
}

fn load_registry(path: &Path) -> io::Result<ProjectRegistry> {
    if !path.exists() {
        return Ok(ProjectRegistry::default());
    }
    let bytes = std::fs::read(path)?;
    let registry: ProjectRegistry = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if registry.schema != PROJECTS_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported projects schema {:?}", registry.schema),
        ));
    }
    for project in &registry.projects {
        validate_registered_project(project)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    Ok(registry)
}

fn save_registry(path: &Path, registry: &ProjectRegistry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_unix_ms()));
    let mut owns_temp = false;
    let result: io::Result<()> = (|| {
        let mut file = create_private_temp(&temp)?;
        owns_temp = true;
        use std::io::Write as _;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        owns_temp = false;
        if let Some(parent) = path.parent() {
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();
    if owns_temp {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    cache_registry(path, registry);
    Ok(())
}

#[cfg(unix)]
fn open_private_lock_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_lock_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn create_private_temp(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_temp(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sample_project() -> RegisteredProject {
        build_project(
            "Example-Org/Example-Repo".to_string(),
            "/work/legacy-app".to_string(),
            Some("main".to_string()),
            ProjectHost::Local,
            ProjectOpenMode::Tmux,
        )
        .unwrap()
    }

    #[test]
    fn focused_context_classifies_local_ssh_and_tmux_modes() {
        assert!(matches!(
            classify_focused_context("/work/repo".into(), None, None, None).unwrap(),
            FocusedProjectSource::Local {
                preferred_open: ProjectOpenMode::Regular,
                ..
            }
        ));
        assert!(matches!(
            classify_focused_context(
                "/work/repo".into(),
                None,
                None,
                Some(crate::tmux::TmuxTarget::Local)
            )
            .unwrap(),
            FocusedProjectSource::Local {
                preferred_open: ProjectOpenMode::Tmux,
                ..
            }
        ));
        let ssh = vec!["ssh".into(), "-p".into(), "2222".into(), "dev@host".into()];
        assert!(matches!(
            classify_focused_context(
                "/srv/repo".into(),
                Some("host".into()),
                Some(ssh),
                None
            )
            .unwrap(),
            FocusedProjectSource::Remote {
                ssh_target,
                preferred_open: ProjectOpenMode::Regular,
                ..
            } if ssh_target == "dev@host"
        ));
        assert!(matches!(
            classify_focused_context(
                "/srv/repo".into(),
                Some("host".into()),
                None,
                Some(crate::tmux::TmuxTarget::Remote {
                    ssh_target: "dev@host".into()
                })
            )
            .unwrap(),
            FocusedProjectSource::Remote {
                preferred_open: ProjectOpenMode::RemoteTmux,
                ..
            }
        ));
        assert!(
            classify_focused_context("/srv/repo".into(), Some("host".into()), None, None).is_err()
        );
    }

    #[test]
    fn project_registry_round_trips_versioned_json() {
        let mut registry = ProjectRegistry::default();
        registry.upsert(sample_project());
        let json = serde_json::to_string_pretty(&registry).unwrap();
        assert!(json.contains("taarof.projects.v1"));
        assert!(!json.to_ascii_lowercase().contains("token"));
        let decoded: ProjectRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, registry);
    }

    #[test]
    fn project_registry_updates_existing_project_by_stable_identity() {
        let mut registry = ProjectRegistry::default();
        let original = sample_project();
        let created_at = original.created_at_unix_ms;
        registry.upsert(original.clone());
        registry.projects[0].last_opened_at_unix_ms = 99;
        registry.projects[0].preferred_open = ProjectOpenMode::Coder;
        registry.projects[0].coder = Some(CoderProject {
            workspace_name: "legacy-app-dev".into(),
            template: Some("rust".into()),
            repo_path: "/work/legacy-app".into(),
            create_if_missing: true,
        });
        let mut updated = original;
        updated.display_name = "Taarof".to_string();
        updated.last_branch = Some("feature".to_string());
        updated.created_at_unix_ms = created_at + 100;
        registry.upsert(updated);
        assert_eq!(registry.projects.len(), 1);
        assert_eq!(registry.projects[0].display_name, "Taarof");
        assert_eq!(registry.projects[0].created_at_unix_ms, created_at);
        assert_eq!(registry.projects[0].last_opened_at_unix_ms, 99);
        assert_eq!(registry.projects[0].preferred_open, ProjectOpenMode::Coder);
        assert_eq!(
            registry.projects[0]
                .coder
                .as_ref()
                .map(|coder| coder.workspace_name.as_str()),
            Some("legacy-app-dev")
        );
    }

    #[test]
    fn register_project_captures_local_git_identity_without_secrets() {
        let root = std::env::temp_dir().join(format!(
            "taarof-project-local-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/Example-Org/Example-Repo.git",
        ]);
        let project =
            project_from_local_checkout(root.to_str().unwrap(), ProjectOpenMode::Regular).unwrap();
        assert_eq!(project.canonical_repo, "example-org/example-repo");
        assert_eq!(project.host, ProjectHost::Local);
        let json = serde_json::to_string(&project)
            .unwrap()
            .to_ascii_lowercase();
        assert!(!json.contains("credential"));
        assert!(!json.contains("token"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn register_project_captures_remote_git_identity_without_raw_ssh_output() {
        let raw = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/legacy-app\0main\0",
            "origin\0git@github.com:Example-Org/Example-Repo.git\0",
            "git@github.com:Example-Org/Example-Repo.git\0\0\0",
            "git@github.com:Example-Org/Example-Repo.git\0\0\0"
        );
        let crate::tracking::RemoteGitIdentityOutcome::GitHub(identity) =
            crate::tracking::classify_remote_git_identity(Ok(raw.to_string()))
        else {
            panic!("fixture should classify")
        };
        let ssh_argv = vec![
            "ssh".to_string(),
            "-p".to_string(),
            "2222".to_string(),
            "developer@build.example.invalid".to_string(),
        ];
        let project = project_from_remote_identity(
            "build-host",
            "developer@build.example.invalid",
            Some(&ssh_argv),
            identity,
            ProjectOpenMode::RemoteTmux,
        )
        .unwrap();
        assert_eq!(project.checkout_root, "/srv/legacy-app");
        assert_eq!(
            project.canonical_repo.to_ascii_lowercase(),
            "example-org/example-repo"
        );
        let json = serde_json::to_string(&project).unwrap();
        assert!(!json.contains("TAAROF_REMOTE_GIT_IDENTITY"));
        assert!(!json.contains("git@github.com"));
        assert!(json.contains("2222"));
    }

    #[test]
    fn project_registry_persists_versioned_private_json() {
        let root = std::env::temp_dir().join(format!(
            "taarof-project-store-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        let path = root.join("taarof/projects.json");
        let mut registry = ProjectRegistry::default();
        registry.upsert(sample_project());
        save_registry(&path, &registry).unwrap();
        assert_eq!(load_registry(&path).unwrap(), registry);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_open_mode_rejects_local_remote_tmux_and_remote_local_tmux_mismatches() {
        let mut local = sample_project();
        local.preferred_open = ProjectOpenMode::RemoteTmux;
        assert!(build_open_plan(&local).is_err());

        let mut remote = sample_project();
        remote.host = ProjectHost::Ssh {
            name: "dev".into(),
            target: "dev@example.test".into(),
            connection_argv: None,
        };
        remote.preferred_open = ProjectOpenMode::Tmux;
        assert!(build_open_plan(&remote).is_err());
        remote.preferred_open = ProjectOpenMode::RemoteTmux;
        assert!(matches!(
            build_open_plan(&remote).unwrap(),
            ProjectOpenPlan::Remote { tmux: true, .. }
        ));
    }

    #[test]
    fn project_open_commands_preserve_remote_paths_without_shell_injection() {
        let connection = vec![
            "ssh".to_string(),
            "-p".to_string(),
            "2222".to_string(),
            "dev@example.test".to_string(),
        ];
        let argv = remote_shell_argv(
            "dev@example.test",
            Some(&connection),
            "/srv/project with space",
        )
        .unwrap();
        assert_eq!(argv[0], "ssh");
        assert_eq!(&argv[1..3], ["-p", "2222"]);
        assert!(argv.iter().any(|argument| argument == "dev@example.test"));
        assert!(argv.last().unwrap().contains("'/srv/project with space'"));
        assert!(remote_shell_argv("dev@example.test;touch /tmp/no", None, "/srv/project").is_err());
    }

    #[test]
    fn coder_workspace_list_parser_accepts_supported_shapes() {
        assert!(coder_workspace_exists(r#"[{"name":"legacy-app"}]"#, "legacy-app").unwrap());
        assert!(coder_workspace_exists(
            r#"{"workspaces":[{"latest_build":{"workspace_name":"legacy-app"}}]}"#,
            "legacy-app"
        )
        .unwrap());
        assert!(!coder_workspace_exists("[]", "legacy-app").unwrap());
        assert!(coder_workspace_exists("not-json", "legacy-app").is_err());
    }

    #[test]
    fn coder_existing_workspace_plan_does_not_require_creation_template() {
        let mut project = sample_project();
        project.preferred_open = ProjectOpenMode::Coder;
        project.coder = Some(CoderProject {
            workspace_name: "existing".to_string(),
            template: None,
            repo_path: "/workspaces/legacy-app".to_string(),
            create_if_missing: true,
        });
        let ProjectOpenPlan::Coder {
            create_if_missing,
            create_command,
            ..
        } = build_open_plan(&project).unwrap()
        else {
            panic!("expected Coder plan")
        };
        assert!(create_if_missing);
        assert!(create_command.is_none());
    }

    #[test]
    fn coder_project_open_command_uses_cli_without_stored_credentials() {
        let mut project = sample_project();
        project.preferred_open = ProjectOpenMode::Coder;
        project.coder = Some(CoderProject {
            workspace_name: "legacy-app-dev".to_string(),
            template: Some("rust-dev".to_string()),
            repo_path: "/workspaces/legacy-app with space".to_string(),
            create_if_missing: true,
        });
        let ProjectOpenPlan::Coder {
            list_command,
            create_if_missing,
            create_command,
            connect_command,
        } = build_open_plan(&project).unwrap()
        else {
            panic!("expected coder plan")
        };
        assert_eq!(list_command, ["coder", "list", "--output", "json"]);
        assert!(create_if_missing);
        assert_eq!(create_command.unwrap()[0], "coder");
        assert_eq!(connect_command[0], "coder");
        assert_eq!(connect_command[1], "ssh");
        let persisted = serde_json::to_string(&project)
            .unwrap()
            .to_ascii_lowercase();
        assert!(!persisted.contains("token"));
        assert!(!persisted.contains("password"));
    }
}
