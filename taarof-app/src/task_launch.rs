//! Widget-free planning for every mise task launch surface.
//!
//! A task launch is deliberately limited to opening a dedicated terminal tab.
//! Keeping that choice in this module makes it impossible for a new palette,
//! sidebar, panel, or keybinding caller to accidentally feed a command into
//! the terminal that happened to be focused when the action was invoked.

use std::fmt;
use std::path::Path;

use crate::mise::DiscoveryTarget;
use crate::workspace::{ExitRespawn, WorkspaceAction};

/// The UI or binding that asked to run a task.
///
/// This is data rather than a widget type so planning and failure handling are
/// headless-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskLaunchSurface {
    Palette,
    Sidebar,
    Inspector,
    TaskPanel,
    BindableAction,
}

impl TaskLaunchSurface {
    fn label(self) -> &'static str {
        match self {
            Self::Palette => "command palette",
            Self::Sidebar => "sidebar",
            Self::Inspector => "workspace inspector",
            Self::TaskPanel => "task panel",
            Self::BindableAction => "task shortcut",
        }
    }
}

/// Immutable identity captured when a task affordance is rendered.
///
/// The target is deliberately paired with the *complete* discovered task-name
/// set. A label alone is not a launch capability: the planner must later prove
/// that the pane still resolves to the same target and that this task was part
/// of that target's discovery result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskDiscoverySnapshot {
    target: DiscoveryTarget,
    task_names: Vec<String>,
}

impl TaskDiscoverySnapshot {
    pub(crate) fn from_tasks(target: DiscoveryTarget, tasks: &[crate::mise::MiseTask]) -> Self {
        let mut task_names: Vec<String> = tasks.iter().map(|task| task.name.clone()).collect();
        task_names.sort();
        task_names.dedup();
        Self { target, task_names }
    }

    #[cfg(test)]
    pub(crate) fn with_task_names(
        target: DiscoveryTarget,
        task_names: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut task_names: Vec<String> = task_names.into_iter().collect();
        task_names.sort();
        task_names.dedup();
        Self { target, task_names }
    }

    pub(crate) fn target(&self) -> &DiscoveryTarget {
        &self.target
    }

    pub(crate) fn contains_task(&self, task_name: &str) -> bool {
        self.task_names
            .binary_search_by(|candidate| candidate.as_str().cmp(task_name))
            .is_ok()
    }
}

/// A typed request to run one discovered mise task.
#[derive(Debug, Clone)]
pub(crate) struct TaskLaunchRequest {
    source: TaskLaunchSurface,
    source_tab_id: u32,
    discovery: Option<TaskDiscoverySnapshot>,
    task_name: String,
    workspace_action: Option<WorkspaceAction>,
}

impl TaskLaunchRequest {
    /// Build the production callback payload from the tab that rendered an
    /// affordance. Every palette/sidebar/inspector/task-panel/chord callback
    /// calls this before reaching the shared planner.
    pub(crate) fn for_discovered_tab(
        source: TaskLaunchSurface,
        state: &crate::AppState,
        source_tab_id: u32,
        task_name: impl Into<String>,
        workspace_action: Option<WorkspaceAction>,
    ) -> Self {
        Self::from_snapshot(
            source,
            source_tab_id,
            state.task_discovery_snapshots.get(&source_tab_id).cloned(),
            task_name,
            workspace_action,
        )
    }

    pub(crate) fn from_snapshot(
        source: TaskLaunchSurface,
        source_tab_id: u32,
        discovery: Option<TaskDiscoverySnapshot>,
        task_name: impl Into<String>,
        workspace_action: Option<WorkspaceAction>,
    ) -> Self {
        Self {
            source,
            source_tab_id,
            discovery,
            task_name: task_name.into(),
            workspace_action,
        }
    }

    /// Resolve a request into the only task-launch effect the product allows:
    /// starting a command in a dedicated task tab.
    pub(crate) fn plan(
        &self,
        current_target: Option<DiscoveryTarget>,
    ) -> Result<TaskLaunchPlan, TaskLaunchError> {
        let task_name = self.task_name.trim();
        if task_name.is_empty() {
            return Err(TaskLaunchError::MissingTaskName {
                source: self.source,
            });
        }

        let discovery = self
            .discovery
            .as_ref()
            .ok_or(TaskLaunchError::NoDiscoverySnapshot {
                source: self.source,
            })?;
        let current_target = current_target.ok_or(TaskLaunchError::NoTarget {
            source: self.source,
        })?;
        if !crate::mise::same_task_target(discovery.target(), &current_target) {
            return Err(TaskLaunchError::StaleDiscoveryTarget {
                source: self.source,
                task_name: task_name.to_string(),
                discovered_target: crate::mise::task_target_label(discovery.target()),
                current_target: crate::mise::task_target_label(&current_target),
            });
        }
        if !discovery.contains_task(task_name) {
            return Err(TaskLaunchError::TaskUnavailable {
                source: self.source,
                task_name: task_name.to_string(),
            });
        }
        validate_target(self.source, &current_target)?;

        let (working_dir, argv) = crate::mise::task_spawn_config(&current_target, task_name)
            .ok_or_else(|| match &current_target {
                DiscoveryTarget::Local { cwd, .. } => {
                    TaskLaunchError::LocalMiseUnavailable { cwd: cwd.clone() }
                }
                DiscoveryTarget::Remote { host, cwd, .. } => {
                    TaskLaunchError::RemoteConnectionUnavailable {
                        host: host.clone(),
                        cwd: cwd.clone(),
                    }
                }
            })?;
        if matches!(&current_target, DiscoveryTarget::Local { .. })
            && !argv
                .first()
                .is_some_and(|program| local_tool_is_executable(Path::new(program)))
        {
            let DiscoveryTarget::Local { cwd, .. } = &current_target else {
                unreachable!("local argv validation is only reached for local targets");
            };
            return Err(TaskLaunchError::LocalMiseUnavailable { cwd: cwd.clone() });
        }

        let tab_name = self
            .workspace_action
            .map(|action| action.label().to_string())
            .unwrap_or_else(|| format!("mise: {task_name}"));
        Ok(TaskLaunchPlan {
            tab_name,
            working_dir,
            argv,
            respawn_on_exit: crate::mise::task_post_exit_respawn(&current_target),
            workspace_action: self.workspace_action,
            target_label: crate::mise::task_target_label(&current_target),
        })
    }

    pub(crate) fn plan_for_current_tab(
        &self,
        state: &crate::AppState,
    ) -> Result<TaskLaunchPlan, TaskLaunchError> {
        self.plan(crate::mise::task_target_for_tab(state, self.source_tab_id))
    }

    /// Execute through a narrow seam that has no focused-terminal write API.
    #[cfg(test)]
    pub(crate) fn dispatch(
        &self,
        current_target: Option<DiscoveryTarget>,
        executor: &mut impl TaskLaunchExecutor,
    ) -> Result<(), TaskLaunchError> {
        executor.open_dedicated_task_tab(self.plan(current_target)?)
    }

    /// Test-only dispatch hook used by the entry-surface harness. The observer
    /// runs immediately after the real planner returns and before the only
    /// permitted effect (creating one dedicated task tab).
    #[cfg(test)]
    pub(crate) fn dispatch_observed(
        &self,
        current_target: Option<DiscoveryTarget>,
        executor: &mut impl TaskLaunchExecutor,
        observe_plan: impl FnOnce(&TaskLaunchPlan),
    ) -> Result<(), TaskLaunchError> {
        let plan = self.plan(current_target)?;
        observe_plan(&plan);
        executor.open_dedicated_task_tab(plan)
    }
}

fn validate_target(
    source: TaskLaunchSurface,
    target: &DiscoveryTarget,
) -> Result<(), TaskLaunchError> {
    match target {
        DiscoveryTarget::Local { cwd, .. } if cwd.trim().is_empty() => {
            Err(TaskLaunchError::MissingWorkingDirectory {
                source,
                target: "local project".to_string(),
            })
        }
        DiscoveryTarget::Local { cwd, .. } if !Path::new(cwd).is_dir() => {
            Err(TaskLaunchError::LocalWorkingDirectoryUnavailable { cwd: cwd.clone() })
        }
        DiscoveryTarget::Local {
            cwd,
            binary_path: Some(binary_path),
        } if !local_tool_is_executable(binary_path) => {
            Err(TaskLaunchError::LocalMiseUnavailable { cwd: cwd.clone() })
        }
        DiscoveryTarget::Remote { host, .. } if host.trim().is_empty() => {
            Err(TaskLaunchError::MissingRemoteHost { source })
        }
        DiscoveryTarget::Remote { host, cwd, .. } if cwd.trim().is_empty() => {
            Err(TaskLaunchError::MissingWorkingDirectory {
                source,
                target: host.clone(),
            })
        }
        DiscoveryTarget::Remote {
            host,
            cwd,
            ssh_argv,
        } if ssh_argv
            .first()
            .is_none_or(|program| !command_is_executable(program)) =>
        {
            Err(TaskLaunchError::RemoteConnectionUnavailable {
                host: host.clone(),
                cwd: cwd.clone(),
            })
        }
        _ => Ok(()),
    }
}

#[cfg(unix)]
fn local_tool_is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.is_file()
        && std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn command_is_executable(program: &str) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return local_tool_is_executable(path);
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| local_tool_is_executable(&dir.join(program)))
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn local_tool_is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Fully resolved dedicated-task-tab spawn data.
#[derive(Debug, Clone)]
pub(crate) struct TaskLaunchPlan {
    pub(crate) tab_name: String,
    pub(crate) working_dir: Option<String>,
    pub(crate) argv: Vec<String>,
    pub(crate) respawn_on_exit: Option<ExitRespawn>,
    pub(crate) workspace_action: Option<WorkspaceAction>,
    pub(crate) target_label: String,
}

/// The only runtime effect a task planner can request.
///
/// Intentionally no method accepts a VTE, pane id, or byte slice. The GTK
/// adapter can create a task tab, but cannot turn a task affordance back into
/// a focused-shell injection.
pub(crate) trait TaskLaunchExecutor {
    fn open_dedicated_task_tab(&mut self, plan: TaskLaunchPlan) -> Result<(), TaskLaunchError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskLaunchError {
    MissingTaskName {
        source: TaskLaunchSurface,
    },
    TaskUnavailable {
        source: TaskLaunchSurface,
        task_name: String,
    },
    NoDiscoverySnapshot {
        source: TaskLaunchSurface,
    },
    NoTarget {
        source: TaskLaunchSurface,
    },
    StaleDiscoveryTarget {
        source: TaskLaunchSurface,
        task_name: String,
        discovered_target: String,
        current_target: String,
    },
    MissingWorkingDirectory {
        source: TaskLaunchSurface,
        target: String,
    },
    LocalWorkingDirectoryUnavailable {
        cwd: String,
    },
    MissingRemoteHost {
        source: TaskLaunchSurface,
    },
    LocalMiseUnavailable {
        cwd: String,
    },
    RemoteConnectionUnavailable {
        host: String,
        cwd: String,
    },
    TaskTabStartFailed {
        target: String,
        reason: String,
    },
}

impl fmt::Display for TaskLaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTaskName { source } => {
                write!(f, "The {} did not identify a task to run.", source.label())
            }
            Self::TaskUnavailable { source, task_name } => write!(
                f,
                "Task '{task_name}' is no longer in this tab's discovered task set. Use Discover Tasks before invoking it from the {}.",
                source.label()
            ),
            Self::NoDiscoverySnapshot { source } => write!(
                f,
                "The {} has no current task discovery result. Use Discover Tasks for this tab, then retry.",
                source.label()
            ),
            Self::NoTarget { source } => write!(
                f,
                "Could not resolve a project target for the {}. Focus a terminal tab with a project working directory and discover its tasks.",
                source.label()
            ),
            Self::StaleDiscoveryTarget {
                source,
                task_name,
                discovered_target,
                current_target,
            } => write!(
                f,
                "Task '{task_name}' was discovered for {discovered_target}, but this tab now targets {current_target}. Use Discover Tasks, then retry from the {}.",
                source.label()
            ),
            Self::MissingWorkingDirectory { source, target } => write!(
                f,
                "Could not resolve a working directory for {target} from the {}. Focus a terminal after it reports its directory, then retry.",
                source.label()
            ),
            Self::MissingRemoteHost { source } => write!(
                f,
                "Could not resolve the remote host for the {}. Reconnect the SSH pane, then discover tasks again.",
                source.label()
            ),
            Self::LocalMiseUnavailable { cwd } => write!(
                f,
                "mise is unavailable for {cwd}. Install mise or set MISE_BIN, then rediscover tasks."
            ),
            Self::LocalWorkingDirectoryUnavailable { cwd } => write!(
                f,
                "The task working directory {cwd} is no longer available. Return to the project and rediscover tasks."
            ),
            Self::RemoteConnectionUnavailable { host, cwd } => write!(
                f,
                "Could not prepare a task tab for {host}:{cwd}. Reconnect the SSH pane and ensure its SSH command is available, then rediscover tasks."
            ),
            Self::TaskTabStartFailed { target, reason } => write!(
                f,
                "Could not start the dedicated task tab for {target}: {reason}. No task tab was kept; retry after fixing the launch environment."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TaskDiscoverySnapshot, TaskLaunchError, TaskLaunchExecutor, TaskLaunchRequest,
        TaskLaunchSurface,
    };
    use crate::mise::DiscoveryTarget;
    use std::path::PathBuf;

    #[derive(Default)]
    struct RecordingExecutor {
        task_tab_launches: Vec<super::TaskLaunchPlan>,
        dedicated_tab_invocations: usize,
        focused_terminal_bytes: Vec<u8>,
    }

    impl TaskLaunchExecutor for RecordingExecutor {
        fn open_dedicated_task_tab(
            &mut self,
            plan: super::TaskLaunchPlan,
        ) -> Result<(), TaskLaunchError> {
            self.dedicated_tab_invocations += 1;
            self.task_tab_launches.push(plan);
            Ok(())
        }
    }

    struct FailingExecutor;

    impl TaskLaunchExecutor for FailingExecutor {
        fn open_dedicated_task_tab(
            &mut self,
            plan: super::TaskLaunchPlan,
        ) -> Result<(), TaskLaunchError> {
            Err(TaskLaunchError::TaskTabStartFailed {
                target: plan.target_label,
                reason: "broker refused the child".to_string(),
            })
        }
    }

    fn local_target() -> DiscoveryTarget {
        DiscoveryTarget::Local {
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            // A host-provided executable is a stable fixture: it tests planner
            // validation without depending on a shell script or PATH lookup.
            binary_path: Some(PathBuf::from("/usr/bin/true")),
        }
    }

    fn request(
        surface: TaskLaunchSurface,
        target: DiscoveryTarget,
        task_name: &str,
        workspace_action: Option<crate::WorkspaceAction>,
    ) -> TaskLaunchRequest {
        TaskLaunchRequest::from_snapshot(
            surface,
            7,
            Some(TaskDiscoverySnapshot::with_task_names(
                target,
                ["dev".to_string(), "test".to_string()],
            )),
            task_name,
            workspace_action,
        )
    }

    #[test]
    fn test_all_task_surfaces_choose_a_dedicated_tab() {
        for surface in [
            TaskLaunchSurface::Palette,
            TaskLaunchSurface::Sidebar,
            TaskLaunchSurface::Inspector,
            TaskLaunchSurface::TaskPanel,
            TaskLaunchSurface::BindableAction,
        ] {
            let request = request(
                surface,
                local_target(),
                "dev",
                (surface == TaskLaunchSurface::BindableAction)
                    .then_some(crate::WorkspaceAction::Dev),
            );
            let mut executor = RecordingExecutor::default();

            request
                .dispatch(Some(local_target()), &mut executor)
                .expect("every task surface should plan one dedicated task tab");

            assert_eq!(executor.task_tab_launches.len(), 1, "{surface:?}");
            assert_eq!(executor.dedicated_tab_invocations, 1, "{surface:?}");
            assert!(
                !executor.task_tab_launches[0].argv.is_empty(),
                "{surface:?} must record one planned task command"
            );
            assert!(
                executor.focused_terminal_bytes.is_empty(),
                "{surface:?} must never write to the previously focused VTE"
            );
        }
    }

    #[test]
    fn entry_surface_harness_activates_production_adapters_and_workspace_gaction() {
        use gio::prelude::ActionExt;
        use std::cell::RefCell;
        use std::rc::Rc;

        let target = local_target();
        let snapshot = TaskDiscoverySnapshot::with_task_names(target.clone(), ["dev".to_string()]);
        let mut state = crate::AppState::new();
        let workspace_id = state.workspaces[0].id;
        let (source_tab_id, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "task launch harness",
            crate::HeadlessPaneSeed {
                cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("harness needs a real active-tab callback context");
        state
            .task_discovery_snapshots
            .insert(source_tab_id, snapshot.clone());

        let palette = crate::palette::palette_task_request(
            source_tab_id,
            Some(snapshot.clone()),
            "dev",
            None,
        );
        let sidebar = crate::sidebar::sidebar_task_launch_request(&state, source_tab_id, "dev");
        let inspector =
            crate::inspector::inspector_task_launch_request(&state, source_tab_id, "dev");
        let task_panel =
            crate::task_panel::task_panel_task_launch_request(&state, source_tab_id, "dev");

        for (name, request) in [
            ("palette", palette),
            ("sidebar", sidebar),
            ("inspector", inspector),
            ("task panel", task_panel),
        ] {
            let mut executor = RecordingExecutor::default();
            let mut planner_invocations = 0;
            request
                .dispatch_observed(Some(target.clone()), &mut executor, |_| {
                    planner_invocations += 1;
                })
                .unwrap_or_else(|error| panic!("{name} adapter should dispatch: {error}"));
            assert_eq!(
                planner_invocations, 1,
                "{name} must not bypass or duplicate planning"
            );
            assert_eq!(
                executor.task_tab_launches.len(),
                1,
                "{name} must open one dedicated tab"
            );
            assert_eq!(
                executor.dedicated_tab_invocations, 1,
                "{name} must create exactly one dedicated tab"
            );
            assert!(
                executor.focused_terminal_bytes.is_empty(),
                "{name} must not write to a prior VTE"
            );
        }

        // This is a real GAction activation, but intentionally the existing
        // `quick-action` vocabulary -- never a synthetic `win.run-*` action.
        let state = Rc::new(RefCell::new(state));
        let request_from_gaction = Rc::new(RefCell::new(None));
        let gaction = gio::SimpleAction::new("quick-action", None);
        {
            let state = state.clone();
            let request_from_gaction = request_from_gaction.clone();
            gaction.connect_activate(move |_, _| {
                let target =
                    crate::keybindings::ChordTarget::Workspace(crate::WorkspaceAction::Dev);
                if let crate::keybindings::ChordTarget::Workspace(action) = target {
                    *request_from_gaction.borrow_mut() = Some(
                        crate::workspace_chord_task_launch_request(&state.borrow(), action),
                    );
                }
            });
        }
        gaction.activate(None);
        let request = request_from_gaction
            .borrow_mut()
            .take()
            .expect("quick-action GAction must reach its production workspace callback");
        let mut executor = RecordingExecutor::default();
        let mut planner_invocations = 0;
        request
            .dispatch_observed(Some(target.clone()), &mut executor, |_| {
                planner_invocations += 1;
            })
            .expect("workspace quick-action should plan");
        assert_eq!(planner_invocations, 1);
        assert_eq!(executor.task_tab_launches.len(), 1);
        assert_eq!(executor.dedicated_tab_invocations, 1);
        assert!(executor.focused_terminal_bytes.is_empty());

        // A missing discovery identity is an explicit failure, not a fallback
        // injection or a second planner path.
        let missing = crate::sidebar::sidebar_task_launch_request(
            &crate::AppState::new(),
            source_tab_id,
            "dev",
        );
        let mut executor = RecordingExecutor::default();
        let mut planner_invocations = 0;
        assert!(missing
            .dispatch_observed(Some(target), &mut executor, |_| {
                planner_invocations += 1;
            })
            .is_err());
        assert_eq!(planner_invocations, 0);
        assert!(executor.task_tab_launches.is_empty());
        assert_eq!(executor.dedicated_tab_invocations, 0);
        assert!(executor.focused_terminal_bytes.is_empty());
    }

    #[test]
    fn test_target_cwd_tool_and_task_failures_are_actionable() {
        let no_target = TaskLaunchRequest::from_snapshot(
            TaskLaunchSurface::BindableAction,
            7,
            None,
            "dev",
            None,
        );
        assert!(no_target
            .plan(Some(local_target()))
            .expect_err("missing target must fail closed")
            .to_string()
            .contains("Discover Tasks"));

        let missing_target = DiscoveryTarget::Local {
            cwd: " ".into(),
            binary_path: Some("/usr/bin/true".into()),
        };
        let missing_cwd = request(
            TaskLaunchSurface::Palette,
            missing_target.clone(),
            "dev",
            None,
        );
        assert!(missing_cwd
            .plan(Some(missing_target))
            .expect_err("blank local cwd must fail closed")
            .to_string()
            .contains("working directory"));

        let missing_tool_target = DiscoveryTarget::Local {
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            binary_path: Some("/definitely/not/mise".into()),
        };
        let missing_tool = request(
            TaskLaunchSurface::Sidebar,
            missing_tool_target.clone(),
            "dev",
            None,
        );
        assert!(missing_tool
            .plan(Some(missing_tool_target))
            .expect_err("stale local mise path must fail before spawning")
            .to_string()
            .contains("Install mise or set MISE_BIN"));

        let remote_without_connection = DiscoveryTarget::Remote {
            host: "builder".into(),
            cwd: "/srv/app".into(),
            ssh_argv: Vec::new(),
        };
        let missing_remote_connection = request(
            TaskLaunchSurface::TaskPanel,
            remote_without_connection.clone(),
            "test",
            None,
        );
        assert!(missing_remote_connection
            .plan(Some(remote_without_connection))
            .expect_err("remote task needs a reusable SSH command")
            .to_string()
            .contains("Reconnect the SSH pane"));

        let unavailable_task = request(
            TaskLaunchSurface::BindableAction,
            local_target(),
            "build",
            None,
        );
        assert!(unavailable_task
            .plan(Some(local_target()))
            .expect_err("undiscovered bindable task must not start")
            .to_string()
            .contains("discovered task set"));
    }

    #[test]
    fn stale_target_cannot_launch_a_task_from_the_previous_cwd() {
        let request = request(
            TaskLaunchSurface::Sidebar,
            DiscoveryTarget::Local {
                cwd: "/tmp/project-a".into(),
                binary_path: Some("/usr/bin/true".into()),
            },
            "dev",
            None,
        );
        let error = request
            .plan(Some(DiscoveryTarget::Local {
                cwd: "/tmp/project-b".into(),
                binary_path: Some("/usr/bin/true".into()),
            }))
            .expect_err("A task affordance must not launch after A to B target drift");
        assert!(error.to_string().contains("project-a"));
        assert!(error.to_string().contains("project-b"));
    }

    #[test]
    fn executor_failure_is_returned_without_a_second_or_dead_launch() {
        let request = request(TaskLaunchSurface::Palette, local_target(), "dev", None);
        let mut executor = FailingExecutor;
        let error = request
            .dispatch(Some(local_target()), &mut executor)
            .expect_err("broker failure must reach the UI caller");
        assert!(error.to_string().contains("No task tab was kept"));
    }
}
