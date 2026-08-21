use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneTaskBinding {
    pub task_id: String,
    pub title: String,
    /// Canonical checkout root captured at bind time. Older saved sessions omit
    /// this field; those bindings remain visible but deliberately unresolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_root: Option<Box<str>>,
    /// Same-user stale-context nonce for work reports. It proves only that a
    /// report still refers to this exact binding; `.plan/tasks.json` remains
    /// canonical task authority.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reporting_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneTaskStatus {
    pub task_id: String,
    pub title: String,
    pub resolved: bool,
    pub checkout_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneTaskBindingError {
    PaneNotFound,
    NoLocalCwd,
    NoPlanFile,
    PlanUnreadable,
    TaskNotFound,
    InvalidTaskId,
}

pub(crate) fn valid_reporting_token(value: &str) -> bool {
    value.len() == 36
        && value.starts_with("ctx-")
        && value[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn new_reporting_token() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        bytes =
            (now ^ ((u128::from(std::process::id())) << 64) ^ u128::from(counter)).to_le_bytes();
    }
    let mut token = String::with_capacity(36);
    token.push_str("ctx-");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}

pub fn plan_root_from_pane_cwd(cwd: &str) -> Option<PathBuf> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return None;
    }

    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        return None;
    }

    let mut dir = cwd.to_path_buf();
    loop {
        if dir.join(".plan/tasks.json").is_file() && dir.join(".git").exists() {
            return Some(dir);
        }
        if dir.join(".git").exists() {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn checkout_root_from_pane_cwd(cwd: &str) -> Option<PathBuf> {
    plan_root_from_pane_cwd(cwd)
}

fn canonical_checkout_root(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub fn resolve_task_from_pane_cwd(
    cwd: &str,
    task_id: &str,
) -> Result<PaneTaskBinding, PaneTaskBindingError> {
    let root = plan_root_from_pane_cwd(cwd).ok_or(PaneTaskBindingError::NoPlanFile)?;
    let data =
        crate::tracking::PlanTasksData::load(&root).ok_or(PaneTaskBindingError::PlanUnreadable)?;
    let task_id = task_id.trim();
    if !crate::loop_runner::is_safe_issue_id(task_id) {
        return Err(PaneTaskBindingError::InvalidTaskId);
    }
    let task = data
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or(PaneTaskBindingError::TaskNotFound)?;
    Ok(PaneTaskBinding {
        task_id: task.id.clone(),
        title: task.title.clone(),
        checkout_root: Some(canonical_checkout_root(&root).into_boxed_str()),
        reporting_token: new_reporting_token(),
    })
}

pub fn current_task_status(
    cwd: Option<&str>,
    cwd_host: Option<&str>,
    remote_shell: bool,
    binding: Option<&PaneTaskBinding>,
) -> Option<PaneTaskStatus> {
    let binding = binding?;
    let unresolved = || PaneTaskStatus {
        task_id: binding.task_id.clone(),
        title: binding.title.clone(),
        resolved: false,
        checkout_root: binding.checkout_root.as_deref().map(str::to_owned),
    };

    if remote_shell || crate::terminal::is_remote_host(cwd_host) {
        return Some(unresolved());
    }
    let Some(pinned_root) = binding.checkout_root.as_deref() else {
        return Some(unresolved());
    };
    let Some(current_root) = cwd
        .and_then(plan_root_from_pane_cwd)
        .map(|root| canonical_checkout_root(&root))
    else {
        return Some(unresolved());
    };
    if current_root != pinned_root {
        return Some(unresolved());
    }

    match resolve_task_from_pane_cwd(cwd.unwrap_or_default(), &binding.task_id) {
        Ok(task) if task.checkout_root.as_deref() == Some(pinned_root) => Some(PaneTaskStatus {
            task_id: binding.task_id.clone(),
            title: task.title,
            resolved: true,
            checkout_root: binding.checkout_root.as_deref().map(str::to_owned),
        }),
        _ => Some(unresolved()),
    }
}

pub fn current_task_payload(
    cwd: Option<&str>,
    cwd_host: Option<&str>,
    remote_shell: bool,
    binding: Option<&PaneTaskBinding>,
) -> Value {
    let Some(status) = current_task_status(cwd, cwd_host, remote_shell, binding) else {
        return Value::Null;
    };

    json!({
        "id": status.task_id,
        "title": status.title,
        "resolved": status.resolved,
        "checkout_root": status.checkout_root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn unique_test_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "taarof-task-binding-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("test dir should be created");
        root
    }

    fn write_tasks(root: &Path, tasks: &[(&str, &str)]) {
        fs::create_dir_all(root.join(".git")).expect("git dir should be created");
        let plan_dir = root.join(".plan");
        fs::create_dir_all(&plan_dir).expect("plan dir should be created");
        let tasks_json = tasks
            .iter()
            .map(|(id, title)| format!(r#"{{"id":"{id}","title":"{title}","status":"todo"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            plan_dir.join("tasks.json"),
            format!(r#"{{"tasks":[{tasks_json}]}}"#),
        )
        .expect("tasks.json should be written");
    }

    fn write_tasks_without_git(root: &Path, tasks: &[(&str, &str)]) {
        let plan_dir = root.join(".plan");
        fs::create_dir_all(&plan_dir).expect("plan dir should be created");
        let tasks_json = tasks
            .iter()
            .map(|(id, title)| format!(r#"{{"id":"{id}","title":"{title}","status":"todo"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            plan_dir.join("tasks.json"),
            format!(r#"{{"tasks":[{tasks_json}]}}"#),
        )
        .expect("tasks.json should be written");
    }

    #[test]
    fn resolves_only_from_nearest_pane_local_plan() {
        let root = unique_test_dir("nearest-plan");
        write_tasks(&root, &[("ROOT-1", "Root task")]);
        let nested = root.join("nested");
        write_tasks(&nested, &[("NESTED-1", "Nested task")]);
        let pane_cwd = nested.join("src");
        fs::create_dir_all(&pane_cwd).expect("pane cwd should be created");

        let binding = resolve_task_from_pane_cwd(&pane_cwd.to_string_lossy(), "NESTED-1")
            .expect("nearest task should resolve");
        assert_eq!(binding.title, "Nested task");

        assert_eq!(
            resolve_task_from_pane_cwd(&pane_cwd.to_string_lossy(), "ROOT-1"),
            Err(PaneTaskBindingError::TaskNotFound)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unresolved_payload_does_not_fallback_to_absent_plan() {
        let root = unique_test_dir("no-plan");
        let pane_cwd = root.join("src");
        fs::create_dir_all(&pane_cwd).expect("pane cwd should be created");
        let binding = PaneTaskBinding {
            task_id: "EXAMPLE-110".to_string(),
            title: "Stored title".to_string(),
            checkout_root: None,
            reporting_token: new_reporting_token(),
        };

        let payload = current_task_payload(
            Some(&pane_cwd.to_string_lossy()),
            None,
            false,
            Some(&binding),
        );

        assert_eq!(payload["id"], json!("EXAMPLE-110"));
        assert_eq!(payload["title"], json!("Stored title"));
        assert_eq!(payload["resolved"], json!(false));
        assert_eq!(payload["checkout_root"], Value::Null);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plan_discovery_requires_plan_in_exact_git_checkout() {
        let root = unique_test_dir("plan-without-git");
        write_tasks_without_git(&root, &[("EXAMPLE-110", "No checkout")]);
        let pane_cwd = root.join("src");
        fs::create_dir_all(&pane_cwd).expect("pane cwd should be created");

        assert_eq!(plan_root_from_pane_cwd(&pane_cwd.to_string_lossy()), None);
        assert_eq!(
            resolve_task_from_pane_cwd(&pane_cwd.to_string_lossy(), "EXAMPLE-110"),
            Err(PaneTaskBindingError::NoPlanFile)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn binding_rejects_invalid_task_ids_and_rotates_reporting_nonce() {
        let root = unique_test_dir("reporting-token");
        write_tasks(&root, &[("EXAMPLE-112", "Report work")]);
        assert_eq!(
            resolve_task_from_pane_cwd(&root.to_string_lossy(), "EXAMPLE-112;rm"),
            Err(PaneTaskBindingError::InvalidTaskId)
        );
        let first = resolve_task_from_pane_cwd(&root.to_string_lossy(), "EXAMPLE-112").unwrap();
        let second = resolve_task_from_pane_cwd(&root.to_string_lossy(), "EXAMPLE-112").unwrap();
        assert!(valid_reporting_token(&first.reporting_token));
        assert!(valid_reporting_token(&second.reporting_token));
        assert_ne!(first.reporting_token, second.reporting_token);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn linked_worktree_git_file_counts_as_exact_checkout() {
        let root = unique_test_dir("linked-worktree");
        fs::write(root.join(".git"), "gitdir: /tmp/worktrees/project\n")
            .expect("linked worktree git file should be written");
        write_tasks_without_git(&root, &[("EXAMPLE-110", "Linked worktree task")]);
        let pane_cwd = root.join("src");
        fs::create_dir_all(&pane_cwd).expect("pane cwd should be created");

        let binding = resolve_task_from_pane_cwd(&pane_cwd.to_string_lossy(), "EXAMPLE-110")
            .expect("task should resolve from linked worktree checkout");
        assert_eq!(binding.title, "Linked worktree task");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_payload_stays_unresolved_and_reports_only_pinned_checkout() {
        let root = unique_test_dir("remote-payload");
        write_tasks(&root, &[("EXAMPLE-110", "Local task should not resolve")]);
        let binding = PaneTaskBinding {
            task_id: "EXAMPLE-110".to_string(),
            title: "Stored remote title".to_string(),
            checkout_root: Some(canonical_checkout_root(&root).into_boxed_str()),
            reporting_token: new_reporting_token(),
        };

        let payload =
            current_task_payload(Some(&root.to_string_lossy()), None, true, Some(&binding));

        assert_eq!(payload["id"], json!("EXAMPLE-110"));
        assert_eq!(payload["title"], json!("Stored remote title"));
        assert_eq!(payload["resolved"], json!(false));
        assert_eq!(
            payload["checkout_root"],
            json!(canonical_checkout_root(&root))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn payload_stays_pinned_when_pane_moves_to_another_checkout_with_same_task_id() {
        let first = unique_test_dir("drift-first");
        let second = unique_test_dir("drift-second");
        write_tasks(&first, &[("EXAMPLE-110", "Original checkout title")]);
        write_tasks(&second, &[("EXAMPLE-110", "Different checkout title")]);

        let binding = resolve_task_from_pane_cwd(&first.to_string_lossy(), "EXAMPLE-110")
            .expect("original task should bind");
        assert_eq!(
            binding.checkout_root.as_deref(),
            Some(first.canonicalize().unwrap().to_string_lossy().as_ref())
        );

        let payload =
            current_task_payload(Some(&second.to_string_lossy()), None, false, Some(&binding));
        assert_eq!(payload["title"], json!("Original checkout title"));
        assert_eq!(payload["resolved"], json!(false));
        assert_eq!(
            payload["checkout_root"],
            json!(first.canonicalize().unwrap().to_string_lossy())
        );

        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn legacy_binding_without_checkout_identity_is_safe_and_unresolved() {
        let root = unique_test_dir("legacy-binding");
        write_tasks(&root, &[("EXAMPLE-110", "Current checkout title")]);
        let binding: PaneTaskBinding = serde_json::from_value(json!({
            "task_id": "EXAMPLE-110",
            "title": "Stored legacy title"
        }))
        .expect("legacy binding should deserialize");

        let payload = current_task_payload(
            Some(&root.to_string_lossy()),
            Some("localhost"),
            false,
            Some(&binding),
        );
        assert_eq!(payload["title"], json!("Stored legacy title"));
        assert_eq!(payload["resolved"], json!(false));
        assert_eq!(payload["checkout_root"], Value::Null);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_osc7_host_can_resolve_but_true_remote_host_cannot() {
        let root = unique_test_dir("osc7-host");
        write_tasks(&root, &[("EXAMPLE-110", "Local task")]);
        let binding = resolve_task_from_pane_cwd(&root.to_string_lossy(), "EXAMPLE-110")
            .expect("local task should bind");

        let local_hostname = fs::read_to_string("/proc/sys/kernel/hostname")
            .expect("local hostname")
            .trim()
            .to_string();
        for local_host in ["localhost", local_hostname.as_str()] {
            let local = current_task_payload(
                Some(&root.to_string_lossy()),
                Some(local_host),
                false,
                Some(&binding),
            );
            assert_eq!(local["resolved"], json!(true), "host={local_host}");
        }

        let remote = current_task_payload(
            Some(&root.to_string_lossy()),
            Some("remote.example"),
            false,
            Some(&binding),
        );
        assert_eq!(remote["resolved"], json!(false));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plan_discovery_stops_at_nested_checkout_boundary() {
        let root = unique_test_dir("checkout-boundary");
        write_tasks(&root, &[("ROOT-1", "Root task")]);
        let nested = root.join("nested-checkout");
        fs::create_dir_all(nested.join(".git")).expect("nested git dir should be created");
        let pane_cwd = nested.join("src");
        fs::create_dir_all(&pane_cwd).expect("pane cwd should be created");

        assert_eq!(plan_root_from_pane_cwd(&pane_cwd.to_string_lossy()), None);
        assert_eq!(
            resolve_task_from_pane_cwd(&pane_cwd.to_string_lossy(), "ROOT-1"),
            Err(PaneTaskBindingError::NoPlanFile)
        );

        let _ = fs::remove_dir_all(root);
    }
}
