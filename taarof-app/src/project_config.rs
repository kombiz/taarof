use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::views::{SavedView, ViewPreset};

const PROJECT_CONFIG_FILE: &str = ".taarof.config";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    pub agents: ProjectAgentsConfig,
    pub mise: ProjectMiseConfig,
    pub workspace: ProjectWorkspaceConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectAgentsConfig {
    pub signatures: Vec<ProjectAgentSignature>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectMiseConfig {
    pub pinned_tasks: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectWorkspaceConfig {
    pub pinned_dashboard_views: Vec<ViewPreset>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectAgentSignature {
    pub name: String,
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectConfig {
    #[serde(default)]
    agents: RawProjectAgentsConfig,
    #[serde(default)]
    mise: RawProjectMiseConfig,
    #[serde(default)]
    workspace: RawProjectWorkspaceConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectAgentsConfig {
    #[serde(default)]
    signatures: Vec<ProjectAgentSignature>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectMiseConfig {
    #[serde(default)]
    pinned_tasks: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProjectWorkspaceConfig {
    #[serde(default)]
    pinned_dashboard_views: Vec<ViewPreset>,
}

pub fn load_for_workspace_cwd(cwd: &str) -> Option<ProjectConfig> {
    let path = discover_path_for_cwd(Path::new(cwd))?;
    load_from_path(&path)
}

#[cfg(test)]
pub fn active_project_config(state: &crate::AppState) -> Option<ProjectConfig> {
    let workspace = state.active_ws()?;
    let context_tab = state
        .active_tab()
        .filter(|tab| tab.kind == crate::TabKind::Terminal)
        .or_else(|| {
            workspace.last_active_tab.and_then(|tab_id| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id && tab.kind == crate::TabKind::Terminal)
            })
        })
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.kind == crate::TabKind::Terminal)
        });
    project_config_for_tab(workspace, context_tab)
}

#[cfg(test)]
pub fn project_config_for_tab(
    workspace: &crate::Workspace,
    tab: Option<&crate::Tab>,
) -> Option<ProjectConfig> {
    let mut cwd_candidates = Vec::new();
    if let Some(tab) = tab {
        let focused = tab
            .panes
            .leaf(tab.focused_pane_id)
            .or_else(|| tab.panes.first_leaf())
            .and_then(|leaf| leaf.local_cwd());
        cwd_candidates.extend([
            focused,
            tab.discovery_cwd
                .as_ref()
                .filter(|cwd| !cwd.trim().is_empty())
                .cloned(),
        ]);
    }
    cwd_candidates.extend([
        workspace
            .working_tree_path
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .cloned(),
        workspace
            .repo_root
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .cloned(),
    ]);

    for cwd in cwd_candidates.into_iter().flatten() {
        if let Some(config) = load_for_workspace_cwd(&cwd) {
            return Some(config);
        }
    }

    None
}

pub fn pinned_dashboard_views(
    global_views: Vec<SavedView>,
    project: Option<&ProjectConfig>,
) -> Vec<SavedView> {
    let Some(project) = project else {
        return global_views;
    };
    if project.workspace.pinned_dashboard_views.is_empty() {
        return global_views;
    }

    let mut remaining = global_views;
    let mut pinned = Vec::new();

    for preset in project.workspace.pinned_dashboard_views.iter().copied() {
        if pinned.iter().any(|view: &SavedView| view.preset == preset) {
            continue;
        }

        if let Some(index) = remaining
            .iter()
            .position(|view| view.name.eq_ignore_ascii_case(preset.default_name()))
        {
            pinned.push(remaining.remove(index));
            continue;
        }

        pinned.push(SavedView {
            name: preset.default_name().to_string(),
            preset,
            limit: None,
        });
    }

    pinned.extend(remaining);
    pinned
}

pub fn pinned_task_names(project: Option<&ProjectConfig>) -> Option<&[String]> {
    let project = project?;
    (!project.mise.pinned_tasks.is_empty()).then_some(project.mise.pinned_tasks.as_slice())
}

fn load_from_path(path: &Path) -> Option<ProjectConfig> {
    let contents = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<RawProjectConfig>(&contents) {
        Ok(raw) => Some(normalize_project_config(raw)),
        Err(err) => {
            eprintln!("taarof: failed to parse {}: {err}", path.to_string_lossy());
            None
        }
    }
}

fn normalize_project_config(raw: RawProjectConfig) -> ProjectConfig {
    ProjectConfig {
        agents: ProjectAgentsConfig {
            signatures: raw
                .agents
                .signatures
                .into_iter()
                .filter_map(normalize_agent_signature)
                .collect(),
        },
        mise: ProjectMiseConfig {
            pinned_tasks: raw
                .mise
                .pinned_tasks
                .into_iter()
                .map(|task| task.trim().to_string())
                .filter(|task| !task.is_empty())
                .collect(),
        },
        workspace: ProjectWorkspaceConfig {
            pinned_dashboard_views: raw.workspace.pinned_dashboard_views,
        },
    }
}

fn normalize_agent_signature(
    mut signature: ProjectAgentSignature,
) -> Option<ProjectAgentSignature> {
    signature.name = signature.name.trim().to_string();
    if signature.name.is_empty() {
        return None;
    }

    signature.patterns = signature
        .patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_string())
        .filter(|pattern| !pattern.is_empty())
        .collect();
    if signature.patterns.is_empty() {
        signature.patterns.push(signature.name.clone());
    }

    Some(signature)
}

fn discover_path_for_cwd(cwd: &Path) -> Option<PathBuf> {
    let start_dir = resolve_start_dir(cwd)?;
    let git_root = git_root_for_dir(&start_dir);
    let mut current = start_dir.clone();

    loop {
        let candidate = current.join(PROJECT_CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if git_root.as_ref().is_some_and(|root| current == *root) {
            return None;
        }
        if !current.pop() {
            return None;
        }
    }
}

fn resolve_start_dir(cwd: &Path) -> Option<PathBuf> {
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(cwd)
    };
    if cwd.is_dir() {
        std::fs::canonicalize(&cwd).ok().or(Some(cwd))
    } else {
        cwd.parent()
            .map(Path::to_path_buf)
            .and_then(|parent| std::fs::canonicalize(&parent).ok().or(Some(parent)))
    }
}

fn git_root_for_dir(dir: &Path) -> Option<PathBuf> {
    crate::git::discover(dir.to_str()?)
        .workspace_root()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn new_test_dir(label: &str) -> PathBuf {
        let id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "taarof-project-config-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("test dir should exist");
        dir
    }

    #[test]
    fn test_project_config_discovery_walks_to_git_root() {
        let repo_root = new_test_dir("discover");
        let nested = repo_root.join("apps/api/src");
        fs::create_dir_all(&nested).expect("nested cwd");
        fs::create_dir_all(repo_root.join(".git")).expect("git root marker");
        fs::write(
            repo_root.join(PROJECT_CONFIG_FILE),
            "[mise]\npinned_tasks = [\"lint\"]\n",
        )
        .expect("write project config");

        let config = load_for_workspace_cwd(nested.to_str().expect("utf-8 path"))
            .expect("config should be discovered");

        assert_eq!(config.mise.pinned_tasks, vec!["lint"]);

        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn test_no_project_config_means_default_behavior() {
        let repo_root = new_test_dir("missing");
        let nested = repo_root.join("apps/api/src");
        fs::create_dir_all(&nested).expect("nested cwd");
        fs::create_dir_all(repo_root.join(".git")).expect("git root marker");

        let global_views = vec![SavedView {
            name: "Workspace Health".into(),
            preset: ViewPreset::WorkspaceHealth,
            limit: None,
        }];

        assert!(load_for_workspace_cwd(nested.to_str().expect("utf-8 path")).is_none());
        assert_eq!(
            pinned_dashboard_views(global_views.clone(), None),
            global_views
        );

        let _ = fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn active_project_config_skips_empty_dashboard_tab() {
        let repo_root = new_test_dir("dashboard");
        fs::create_dir_all(repo_root.join(".git")).expect("git root marker");
        fs::write(
            repo_root.join(PROJECT_CONFIG_FILE),
            "[workspace]\npinned_dashboard_views = [\"workspace-health\"]\n",
        )
        .expect("write project config");

        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (terminal_id, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Shell",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("terminal tab");
        state
            .find_tab_mut(terminal_id)
            .expect("seeded terminal tab")
            .discovery_cwd = Some(repo_root.to_string_lossy().into_owned());
        let dashboard_id = state.create_dashboard_tab().expect("dashboard tab");

        assert_eq!(state.active_tab().map(|tab| tab.id), Some(terminal_id));
        assert_eq!(state.presented_tab_id(), Some(dashboard_id));
        assert!(matches!(
            state
                .find_tab(dashboard_id)
                .map(|(_, tab)| tab.panes.as_ref()),
            Some(crate::pane::PaneNode::Empty)
        ));
        let config = active_project_config(&state).expect("workspace project config");
        assert_eq!(
            config.workspace.pinned_dashboard_views,
            vec![ViewPreset::WorkspaceHealth]
        );

        let _ = fs::remove_dir_all(&repo_root);
    }
}
