use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::rc::Rc;
#[cfg(test)]
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureEntry {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackingData {
    pub total: usize,
    pub done: usize,
    pub in_progress: usize,
    pub queued: usize,
    pub features: Vec<FeatureEntry>,
    weighted_total: usize,
    weighted_done: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTaskEntry {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub blocked_by: usize,
    pub depends_on: usize,
    pub unresolved_depends_on: usize,
    /// The actual dependency task IDs (not just a count), retained so the panel
    /// can compute parallelization waves and name a blocked task's unmet blockers.
    pub depends_on_ids: Vec<String>,
    /// The model assigned to the task in `.plan/tasks.json`, when present.
    pub assigned_model: Option<String>,
}

impl PlanTaskEntry {
    pub fn is_done(&self) -> bool {
        is_done_status(&self.status)
    }

    pub fn is_in_progress(&self) -> bool {
        is_in_progress_status(&self.status)
    }

    pub fn is_blocked(&self) -> bool {
        !self.is_done() && (self.blocked_by > 0 || self.unresolved_depends_on > 0)
    }

    /// A task is "ready" when it is neither done nor blocked: it can be handed to
    /// the loop runner right now. Mirrors the gating used for the per-task
    /// "Build with loop runner" button.
    pub fn is_ready(&self) -> bool {
        !self.is_done() && !self.is_blocked()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTasksData {
    pub total: usize,
    pub done: usize,
    pub in_progress: usize,
    pub ready: usize,
    pub blocked: usize,
    pub tasks: Vec<PlanTaskEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTaskProjection {
    pub data: PlanTasksData,
    pub plan: ParallelizationPlan,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Freshness {
    Missing,
    Present {
        modified: Option<SystemTime>,
        len: u64,
    },
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct PlanTaskProjectionCache {
    slot: Option<(PathBuf, Freshness, Option<Rc<PlanTaskProjection>>)>,
    reads: u64,
}

#[cfg(test)]
impl PlanTaskProjectionCache {
    pub fn projection(&mut self, checkout_root: &Path) -> Option<Rc<PlanTaskProjection>> {
        let freshness = plan_tasks_freshness(checkout_root);

        if let Some((root, cached_freshness, projection)) = self.slot.as_ref() {
            if root.as_path() == checkout_root && cached_freshness == &freshness {
                return projection.clone();
            }
        }

        self.reads += 1;
        let projection = PlanTasksData::load(checkout_root).map(|data| {
            Rc::new(PlanTaskProjection {
                plan: data.parallelization_plan(),
                data,
            })
        });
        self.slot = Some((checkout_root.to_path_buf(), freshness, projection.clone()));
        projection
    }

    #[cfg(test)]
    fn reads(&self) -> u64 {
        self.reads
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPullRequestEntry {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub url: Option<String>,
    pub head_ref_name: String,
    pub head_repository_owner: String,
    pub base_ref_name: String,
    pub updated_at: Option<String>,
    pub checks: PullRequestChecks,
    pub correlation: Option<PullRequestCorrelationMetadata>,
    pub correlation_marker_present: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum PullRequestChecks {
    #[default]
    None,
    Pending,
    Failed,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestCorrelationMetadata {
    pub task_id: String,
    pub repo: String,
    pub branch: String,
    pub dispatch_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestCorrelation<'a> {
    Linked(&'a BranchPullRequestEntry),
    Ambiguous,
    Unlinked,
    Incomplete,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPullRequestCorrelationMetadata {
    schema: String,
    task_id: String,
    repo: String,
    branch: String,
    dispatch_source: String,
}

const PR_MARKER_PREFIX: &str = "<!-- taarof-work: ";
const PR_MARKER_SENTINEL: &str = "<!-- taarof-work:";
const PR_MARKER_SUFFIX: &str = " -->";

fn bounded_marker_value(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max
        && !value.chars().any(char::is_control)
        && value.trim() == value
}

fn valid_github_component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

pub fn github_pr_identity_from_url(url: &str) -> Option<(String, u64)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?.parse::<u64>().ok()?;
    if number == 0
        || parts.next().is_some()
        || !valid_github_component(owner, 100)
        || !valid_github_component(repo, 100)
    {
        return None;
    }
    Some((format!("{owner}/{repo}").to_ascii_lowercase(), number))
}

pub fn parse_pull_request_correlation_metadata(
    body: &str,
    pr_url: &str,
    github_head_owner: &str,
    github_head_branch: &str,
) -> Option<PullRequestCorrelationMetadata> {
    if body.len() > 100_000 {
        return None;
    }
    if body.match_indices(PR_MARKER_SENTINEL).count() != 1 {
        return None;
    }
    let markers: Vec<&str> = body
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix(PR_MARKER_PREFIX)?
                .strip_suffix(PR_MARKER_SUFFIX)
        })
        .collect();
    if markers.len() != 1 || markers[0].len() > 1_024 {
        return None;
    }
    let raw: RawPullRequestCorrelationMetadata = serde_json::from_str(markers[0]).ok()?;
    let repo = raw.repo.to_ascii_lowercase();
    let (actual_repo, _) = github_pr_identity_from_url(pr_url)?;
    if raw.schema != "taarof.pr-work.v1"
        || !bounded_marker_value(&raw.task_id, 128)
        || !bounded_marker_value(&raw.repo, 201)
        || !bounded_marker_value(&raw.branch, 256)
        || !bounded_marker_value(&raw.dispatch_source, 64)
        || !matches!(
            raw.dispatch_source.as_str(),
            "taarof" | "loop-runner" | "manual" | "agent"
        )
        || !raw
            .task_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        || raw.repo != repo
        || repo != actual_repo
        || !valid_github_component(github_head_owner, 100)
        || raw.branch != github_head_branch
        || !raw
            .dispatch_source
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return None;
    }
    Some(PullRequestCorrelationMetadata {
        task_id: raw.task_id,
        repo,
        branch: raw.branch,
        dispatch_source: raw.dispatch_source,
    })
}

fn pull_request_checks(rollup: &[serde_json::Value]) -> PullRequestChecks {
    if rollup.is_empty() {
        return PullRequestChecks::None;
    }
    let mut pending = false;
    let mut failed = false;
    for check in rollup {
        let state = check
            .get("conclusion")
            .or_else(|| check.get("state"))
            .or_else(|| check.get("status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        match state.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" => failed = true,
            _ => pending = true,
        }
    }
    if failed {
        PullRequestChecks::Failed
    } else if pending {
        PullRequestChecks::Pending
    } else {
        PullRequestChecks::Ready
    }
}

fn pull_request_head_owner(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("login").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|value| bounded_marker_value(value, 100))
        .map(|value| value.to_ascii_lowercase())
}

pub fn correlate_pull_request<'a>(
    task_id: &str,
    base_repository: &str,
    head_owner: &str,
    branch: &str,
    data: &'a BranchPullRequestsData,
) -> PullRequestCorrelation<'a> {
    if !data.query_complete {
        return PullRequestCorrelation::Incomplete;
    }
    let base_repository = base_repository.to_ascii_lowercase();
    let head_owner = head_owner.to_ascii_lowercase();
    let explicit: Vec<_> = data
        .pull_requests
        .iter()
        .filter(|pr| {
            pr.correlation.as_ref().is_some_and(|metadata| {
                metadata.task_id == task_id
                    && metadata.repo == base_repository
                    && metadata.branch == branch
                    && pr.head_ref_name == branch
                    && pr.head_repository_owner == head_owner
            })
        })
        .collect();
    match explicit.as_slice() {
        [pr] => return PullRequestCorrelation::Linked(pr),
        [] => {}
        _ => return PullRequestCorrelation::Ambiguous,
    }
    PullRequestCorrelation::Unlinked
}

impl BranchPullRequestEntry {
    pub fn status_label(&self) -> String {
        if self.is_draft {
            return "draft".to_string();
        }
        let state = normalize_pr_state(&self.state);
        let Some(decision) = self
            .review_decision
            .as_deref()
            .map(normalize_review_decision)
            .filter(|decision| !decision.is_empty())
        else {
            return state;
        };
        format!("{state} · {decision}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPullRequestsData {
    pub total: usize,
    pub open: usize,
    pub draft: usize,
    pub merged: usize,
    pub closed: usize,
    pub pull_requests: Vec<BranchPullRequestEntry>,
    pub query_complete: bool,
}

pub const PULL_REQUEST_QUERY_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize)]
struct FeaturesFile {
    #[serde(default)]
    features: Vec<RawFeature>,
}

#[derive(Debug, Deserialize)]
struct RawFeature {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TasksFile {
    #[serde(default)]
    tasks: Vec<RawPlanTask>,
}

#[derive(Debug, Deserialize)]
struct RawPlanTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    assigned_model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBranchPullRequest {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    head_repository_owner: serde_json::Value,
    #[serde(default)]
    base_ref_name: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    status_check_rollup: Vec<serde_json::Value>,
}

impl TrackingData {
    pub fn load(repo_root: &Path) -> Option<TrackingData> {
        let candidates = [
            repo_root.join(".plan/features.json"),
            repo_root.join(".taarof/features.json"),
        ];

        for path in candidates {
            if let Some(data) = Self::load_from_path(&path) {
                return Some(data);
            }
        }

        None
    }

    fn load_from_path(path: &Path) -> Option<TrackingData> {
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(path).ok()?;
        let parsed: FeaturesFile = serde_json::from_str(&content).ok()?;

        let mut features = Vec::with_capacity(parsed.features.len());
        let mut done = 0usize;
        let mut in_progress = 0usize;
        let mut queued = 0usize;
        let mut weighted_total = 0usize;
        let mut weighted_done = 0usize;

        for feature in parsed.features {
            let status = feature.status.trim().to_string();
            let weight = effort_weight(feature.effort.as_deref());
            weighted_total += weight;

            match status.as_str() {
                "done" => {
                    done += 1;
                    weighted_done += weight;
                }
                "in-progress" => in_progress += 1,
                "queued" => queued += 1,
                _ => {}
            }

            features.push(FeatureEntry {
                id: feature.id,
                title: feature.title,
                status,
            });
        }

        Some(TrackingData {
            total: features.len(),
            done,
            in_progress,
            queued,
            features,
            weighted_total,
            weighted_done,
        })
    }

    pub fn percent_complete(&self) -> f64 {
        if self.weighted_total == 0 {
            return 0.0;
        }

        self.weighted_done as f64 / self.weighted_total as f64
    }
}

impl PlanTasksData {
    pub fn load(repo_root: &Path) -> Option<PlanTasksData> {
        Self::load_from_path(&repo_root.join(".plan/tasks.json"))
    }

    fn load_from_path(path: &Path) -> Option<PlanTasksData> {
        if !path.exists() {
            return None;
        }

        let content = fs::read_to_string(path).ok()?;
        Self::from_json_str(&content)
    }

    /// Parse the contents of a `.plan/tasks.json` file. Shared by the local file
    /// loader and the remote (SSH `cat`) fetch so both go through identical logic.
    pub fn from_json_str(content: &str) -> Option<PlanTasksData> {
        let parsed: TasksFile = serde_json::from_str(content).ok()?;

        let mut tasks = Vec::with_capacity(parsed.tasks.len());
        let mut done = 0usize;
        let mut in_progress = 0usize;
        let mut ready = 0usize;
        let mut blocked = 0usize;
        let done_task_ids: HashSet<String> = parsed
            .tasks
            .iter()
            .filter(|task| is_done_status(&normalize_task_status(&task.status)))
            .map(|task| task.id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();

        for task in parsed.tasks {
            let blocked_by = normalized_id_list(task.blocked_by);
            let depends_on = normalized_id_list(task.depends_on);
            let unresolved_depends_on = depends_on
                .iter()
                .filter(|task_id| !done_task_ids.contains(*task_id))
                .count();
            let entry = PlanTaskEntry {
                id: task.id.trim().to_string(),
                title: task.title.trim().to_string(),
                status: normalize_task_status(&task.status),
                priority: task
                    .priority
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                blocked_by: blocked_by.len(),
                depends_on: depends_on.len(),
                unresolved_depends_on,
                depends_on_ids: depends_on,
                assigned_model: task
                    .assigned_model
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
            };

            if entry.is_done() {
                done += 1;
            } else if entry.is_in_progress() {
                in_progress += 1;
            } else if entry.is_blocked() {
                blocked += 1;
            } else {
                ready += 1;
            }

            tasks.push(entry);
        }

        tasks.sort_by(|left, right| {
            task_status_rank(left)
                .cmp(&task_status_rank(right))
                .then_with(|| {
                    task_priority_rank(left.priority.as_deref())
                        .cmp(&task_priority_rank(right.priority.as_deref()))
                })
                .then_with(|| left.id.cmp(&right.id))
        });

        Some(PlanTasksData {
            total: tasks.len(),
            done,
            in_progress,
            ready,
            blocked,
            tasks,
        })
    }
}

#[cfg(test)]
fn plan_tasks_freshness(checkout_root: &Path) -> Freshness {
    match fs::metadata(checkout_root.join(".plan/tasks.json")) {
        Ok(metadata) => Freshness::Present {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        },
        Err(_) => Freshness::Missing,
    }
}

/// Dependency-aware ordering of a `.plan` backlog, used to render a
/// "Ready now — N can run in parallel" grouping above the ready tasks and to
/// mark blocked tasks with their unmet blockers.
///
/// This is a pure transform over the task ID graph, decoupled from GTK so it can
/// be unit-tested directly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParallelizationPlan {
    /// IDs that can run right now (status != done and no unresolved dependency),
    /// i.e. the first wave. These are independent and can be dispatched in parallel.
    pub ready_now: Vec<String>,
    /// Successive dependency waves. Wave 0 is `ready_now`; each later wave holds
    /// the not-done tasks whose dependencies are all satisfied by earlier waves
    /// (plus already-done tasks). Tasks in the same wave have no dependency on one
    /// another and may run in parallel.
    pub waves: Vec<Vec<String>>,
    /// For each still-blocked task, the unmet (not-done, earlier-than-resolved)
    /// dependency IDs, so the UI can name a task's blockers.
    pub blockers: Vec<(String, Vec<String>)>,
}

impl PlanTasksData {
    /// Compute the ready set and dependency waves over this backlog's ID graph.
    ///
    /// "Done" tasks are treated as already satisfied dependencies. A task is in
    /// wave N when every dependency it lists is either done or resolved in an
    /// earlier wave. Cyclic or externally-unsatisfiable dependencies never enter a
    /// wave and surface as blockers instead.
    pub fn parallelization_plan(&self) -> ParallelizationPlan {
        compute_parallelization(&self.tasks)
    }
}

/// Pure ready-set / wave computation over plan task entries. See
/// [`ParallelizationPlan`]; kept free-standing so tests can drive it with hand-built
/// `PlanTaskEntry` graphs without constructing a full `PlanTasksData`.
pub fn compute_parallelization(tasks: &[PlanTaskEntry]) -> ParallelizationPlan {
    let done_ids: HashSet<&str> = tasks
        .iter()
        .filter(|task| task.is_done())
        .map(|task| task.id.as_str())
        .collect();

    // Only not-done tasks need scheduling; done tasks are pre-satisfied deps.
    let pending: Vec<&PlanTaskEntry> = tasks.iter().filter(|task| !task.is_done()).collect();

    // Dependencies that point at a done task (or a task ID not in this backlog)
    // can never gate scheduling here, so seed "resolved" with the done set and
    // treat unknown dependency IDs as external/satisfied.
    let known_ids: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut resolved: HashSet<String> = done_ids.iter().map(|id| id.to_string()).collect();

    let unmet_for = |task: &PlanTaskEntry, resolved: &HashSet<String>| -> Vec<String> {
        task.depends_on_ids
            .iter()
            .filter(|dep| known_ids.contains(dep.as_str()) && !resolved.contains(*dep))
            .cloned()
            .collect()
    };

    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut remaining: Vec<&PlanTaskEntry> = pending.clone();

    loop {
        let mut wave: Vec<String> = Vec::new();
        let mut next_remaining: Vec<&PlanTaskEntry> = Vec::new();
        for task in &remaining {
            if unmet_for(task, &resolved).is_empty() {
                wave.push(task.id.clone());
            } else {
                next_remaining.push(task);
            }
        }

        // No progress means the rest is cyclic / externally-blocked; stop so we
        // don't loop forever. Those tasks fall through to `blockers` below.
        if wave.is_empty() {
            break;
        }

        for id in &wave {
            resolved.insert(id.clone());
        }
        waves.push(wave);
        remaining = next_remaining;
        if remaining.is_empty() {
            break;
        }
    }

    let ready_now = waves.first().cloned().unwrap_or_default();

    // Anything never scheduled is blocked; report its still-unmet deps.
    let final_resolved = resolved;
    let blockers: Vec<(String, Vec<String>)> = remaining
        .iter()
        .map(|task| (task.id.clone(), unmet_for(task, &final_resolved)))
        .collect();

    ParallelizationPlan {
        ready_now,
        waves,
        blockers,
    }
}

impl BranchPullRequestsData {
    pub fn from_gh_json_str(content: &str) -> Option<BranchPullRequestsData> {
        let parsed: Vec<RawBranchPullRequest> = serde_json::from_str(content).ok()?;
        let query_complete = parsed.len() < PULL_REQUEST_QUERY_LIMIT;
        let mut pull_requests = Vec::with_capacity(parsed.len());
        let mut open = 0usize;
        let mut draft = 0usize;
        let mut merged = 0usize;
        let mut closed = 0usize;

        for pr in parsed {
            let state = normalize_pr_state(&pr.state);
            let checks = pull_request_checks(&pr.status_check_rollup);
            // A deleted fork can leave `headRepositoryOwner` null. It can never
            // satisfy an exact owner/branch match, but it must not make every
            // otherwise valid row in the GitHub response unreadable.
            let Some(head_repository_owner) = pull_request_head_owner(&pr.head_repository_owner)
            else {
                continue;
            };
            let correlation = parse_pull_request_correlation_metadata(
                &pr.body,
                pr.url.as_deref().unwrap_or(""),
                &head_repository_owner,
                &pr.head_ref_name,
            );
            let correlation_marker_present = pr.body.contains(PR_MARKER_SENTINEL);
            if pr.is_draft {
                draft += 1;
            }
            match state.as_str() {
                "open" => open += 1,
                "merged" => merged += 1,
                "closed" => closed += 1,
                _ => {}
            }
            pull_requests.push(BranchPullRequestEntry {
                number: pr.number,
                title: pr.title.trim().to_string(),
                state,
                is_draft: pr.is_draft,
                review_decision: pr
                    .review_decision
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                url: pr
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                head_ref_name: pr.head_ref_name.trim().to_string(),
                head_repository_owner,
                base_ref_name: pr.base_ref_name.trim().to_string(),
                updated_at: pr
                    .updated_at
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                checks,
                correlation,
                correlation_marker_present,
            });
        }

        pull_requests.sort_by(|left, right| {
            pr_status_rank(left)
                .cmp(&pr_status_rank(right))
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| right.number.cmp(&left.number))
        });

        Some(BranchPullRequestsData {
            total: pull_requests.len(),
            open,
            draft,
            merged,
            closed,
            pull_requests,
            query_complete,
        })
    }

    /// Keep only pull requests whose head identity exactly matches the pane's
    /// checkout. `gh pr list --head` accepts a branch name but explicitly does
    /// not support `owner:branch`, so fork ownership must be enforced after the
    /// local GitHub response is parsed.
    pub fn into_exact_head(mut self, head_owner: &str, branch: &str) -> Self {
        self.pull_requests.retain(|pr| {
            pr.head_repository_owner.eq_ignore_ascii_case(head_owner) && pr.head_ref_name == branch
        });
        self.total = self.pull_requests.len();
        self.open = self
            .pull_requests
            .iter()
            .filter(|pr| pr.state == "open")
            .count();
        self.draft = self.pull_requests.iter().filter(|pr| pr.is_draft).count();
        self.merged = self
            .pull_requests
            .iter()
            .filter(|pr| pr.state == "merged")
            .count();
        self.closed = self
            .pull_requests
            .iter()
            .filter(|pr| pr.state == "closed")
            .count();
        self
    }
}

pub fn branch_pull_requests_command(base_repository: &str, branch: &str) -> Vec<String> {
    vec![
        "gh".to_string(),
        "pr".to_string(),
        "list".to_string(),
        "--state".to_string(),
        "all".to_string(),
        "--repo".to_string(),
        base_repository.to_string(),
        "--head".to_string(),
        branch.to_string(),
        "--json".to_string(),
        "number,title,state,isDraft,url,reviewDecision,updatedAt,headRefName,headRepositoryOwner,baseRefName,body,statusCheckRollup"
            .to_string(),
        "--limit".to_string(),
        PULL_REQUEST_QUERY_LIMIT.to_string(),
    ]
}

/// A remote `.plan/tasks.json` location, derived from a pane whose focused shell
/// runs on another host. `ssh_argv` is the exact `ssh …` argv that opened the
/// pane (when known) — the most reliable way to reach the same host, since the
/// OSC 7 `host` alone may not be a resolvable SSH destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTaskTarget {
    pub host: String,
    pub cwd: String,
    pub ssh_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGitIdentity {
    pub checkout_root: String,
    pub branch: String,
    pub head_repository: String,
    pub base_repository: String,
    pub head_owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteGitIdentityOutcome {
    GitHub(RemoteGitIdentity),
    NoRepository,
    DetachedHead {
        checkout_root: String,
        short_commit: String,
    },
    UnsupportedProvider,
    AmbiguousRemotes,
    Error(RemoteGitIdentityError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteGitIdentityError {
    Transport,
    Authentication,
    Permission,
    RootDiscovery,
    Protocol,
}

impl RemoteGitIdentityError {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Authentication => "authentication",
            Self::Permission => "permission",
            Self::RootDiscovery => "root_discovery",
            Self::Protocol => "protocol",
        }
    }
}

const REMOTE_GIT_IDENTITY_PROTOCOL: &str = "TAAROF_REMOTE_GIT_IDENTITY_V1";

const REMOTE_TASKS_PROTOCOL: &str = "TAAROF_REMOTE_TASKS_V1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTasksOutcome {
    Loaded {
        checkout_root: String,
        data: PlanTasksData,
    },
    NoTaskSource {
        checkout_root: Option<String>,
    },
    Error(RemoteTasksError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTasksError {
    Transport,
    Authentication,
    Permission,
    RootDiscovery,
    MalformedJson,
    Schema,
    Protocol,
}

impl RemoteTasksError {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Authentication => "authentication",
            Self::Permission => "permission",
            Self::RootDiscovery => "root_discovery",
            Self::MalformedJson => "malformed_json",
            Self::Schema => "schema",
            Self::Protocol => "protocol",
        }
    }
}

/// Classify the caller-owned result of a non-recording remote task fetch.
///
/// Raw stderr is used only in-memory to distinguish SSH authentication from
/// transport failures. It is intentionally not retained in the outcome so the
/// task panel cannot accidentally copy identities, paths, or SSH options into
/// diagnostics.
pub fn classify_remote_tasks_fetch(result: Result<String, String>) -> RemoteTasksOutcome {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            let lower = error.to_ascii_lowercase();
            let kind = if lower.contains("permission denied (")
                || lower.contains("authentication failed")
                || lower.contains("authentication failure")
                || lower.contains("too many authentication failures")
                || lower.contains("no supported authentication methods")
                || lower.contains("host key verification failed")
            {
                RemoteTasksError::Authentication
            } else {
                RemoteTasksError::Transport
            };
            return RemoteTasksOutcome::Error(kind);
        }
    };

    let mut fields = output.splitn(4, '\0');
    if fields.next() != Some(REMOTE_TASKS_PROTOCOL) {
        return RemoteTasksOutcome::Error(RemoteTasksError::Protocol);
    }
    match fields.next() {
        Some("no-repository") => RemoteTasksOutcome::NoTaskSource {
            checkout_root: None,
        },
        Some("no-plan") => {
            let Some(checkout_root) = fields.next().filter(|root| !root.is_empty()) else {
                return RemoteTasksOutcome::Error(RemoteTasksError::Protocol);
            };
            RemoteTasksOutcome::NoTaskSource {
                checkout_root: Some(checkout_root.to_string()),
            }
        }
        Some("permission") => RemoteTasksOutcome::Error(RemoteTasksError::Permission),
        Some("root-error") => RemoteTasksOutcome::Error(RemoteTasksError::RootDiscovery),
        Some("ok") => {
            let Some(checkout_root) = fields.next().filter(|root| !root.is_empty()) else {
                return RemoteTasksOutcome::Error(RemoteTasksError::Protocol);
            };
            let Some(content) = fields.next() else {
                return RemoteTasksOutcome::Error(RemoteTasksError::Protocol);
            };
            let value = match serde_json::from_str::<serde_json::Value>(content) {
                Ok(value) => value,
                Err(_) => return RemoteTasksOutcome::Error(RemoteTasksError::MalformedJson),
            };
            if serde_json::from_value::<TasksFile>(value).is_err() {
                return RemoteTasksOutcome::Error(RemoteTasksError::Schema);
            }
            let Some(data) = PlanTasksData::from_json_str(content) else {
                return RemoteTasksOutcome::Error(RemoteTasksError::Schema);
            };
            RemoteTasksOutcome::Loaded {
                checkout_root: checkout_root.to_string(),
                data,
            }
        }
        _ => RemoteTasksOutcome::Error(RemoteTasksError::Protocol),
    }
}

/// Build the argv that resolves a remote checkout root and reads its
/// `.plan/tasks.json` over SSH.
///
/// Derives a *connection-only* invocation from the pane's recorded `ssh_argv`
/// (program + connection options + target), preserving any port/identity/jump
/// options but DROPPING the pane's interactive remote command and tty requests,
/// then appends a clean non-interactive root-and-plan probe. The recorded argv is the full
/// `/proc/<pid>/cmdline` of the pane's ssh process (e.g.
/// `ssh -t host 'cd /repo && exec bash'`), so blindly replaying it would mangle
/// the probe. Falls back to a bare `ssh <host>` when no usable argv is recorded.
/// Either way the connection is hardened with `BatchMode=yes` and
/// `ConnectTimeout=5` so it fails fast instead of prompting or hanging, and the
/// remote path is shell-quoted.
pub fn remote_tasks_fetch_command(
    ssh_argv: Option<&[String]>,
    cwd_host: &str,
    remote_cwd: &str,
) -> Vec<String> {
    let remote_cmd = format!(
        "cwd={cwd}; root=$(LC_ALL=C git -C \"$cwd\" rev-parse --show-toplevel 2>/dev/null); git_status=$?; if [ \"$git_status\" -ne 0 ]; then git_error=$(LC_ALL=C git -C \"$cwd\" rev-parse --show-toplevel 2>&1 >/dev/null); case \"$git_error\" in *'not a git repository'*) printf '{protocol}\\0no-repository\\0' ;; *'Permission denied'*|*'detected dubious ownership'*) printf '{protocol}\\0permission\\0' ;; *) printf '{protocol}\\0root-error\\0' ;; esac; exit 0; fi; plan_dir=\"$root/.plan\"; plan=\"$plan_dir/tasks.json\"; if [ ! -e \"$plan_dir\" ] || [ ! -d \"$plan_dir\" ]; then printf '{protocol}\\0no-plan\\0%s\\0' \"$root\"; elif [ ! -x \"$plan_dir\" ]; then printf '{protocol}\\0permission\\0%s\\0' \"$root\"; elif [ ! -e \"$plan\" ]; then printf '{protocol}\\0no-plan\\0%s\\0' \"$root\"; elif [ ! -r \"$plan\" ]; then printf '{protocol}\\0permission\\0%s\\0' \"$root\"; else content=$(cat -- \"$plan\" 2>/dev/null) || {{ printf '{protocol}\\0permission\\0%s\\0' \"$root\"; exit 0; }}; printf '{protocol}\\0ok\\0%s\\0%s' \"$root\" \"$content\"; fi",
        cwd = shell_single_quote(remote_cwd),
        protocol = REMOTE_TASKS_PROTOCOL,
    );

    let mut argv = connection_only_ssh(ssh_argv, cwd_host);
    inject_ssh_batch_options(&mut argv);
    argv.push(remote_cmd);
    argv
}

/// Build the bounded SSH probe used to identify a remote checkout for a local
/// GitHub query. The remote side runs only Git commands and returns repository
/// metadata; GitHub credentials and `gh` never cross the SSH boundary.
pub fn remote_git_identity_command(
    ssh_argv: Option<&[String]>,
    cwd_host: &str,
    remote_cwd: &str,
) -> Vec<String> {
    let remote_cmd = format!(
        "cwd={cwd}; root=$(LC_ALL=C git -C \"$cwd\" rev-parse --show-toplevel 2>/dev/null); git_status=$?; if [ \"$git_status\" -ne 0 ]; then git_error=$(LC_ALL=C git -C \"$cwd\" rev-parse --show-toplevel 2>&1 >/dev/null); case \"$git_error\" in *'not a git repository'*) printf '{protocol}\\0no-repository\\0' ;; *'Permission denied'*|*'detected dubious ownership'*) printf '{protocol}\\0permission\\0' ;; *) printf '{protocol}\\0root-error\\0' ;; esac; exit 0; fi; branch=$(LC_ALL=C git -C \"$root\" symbolic-ref --quiet --short HEAD 2>/dev/null || true); if [ -z \"$branch\" ]; then commit=$(LC_ALL=C git -C \"$root\" rev-parse HEAD 2>/dev/null || true); short_commit=$(printf '%s' \"$commit\" | cut -c1-7); printf '{protocol}\\0detached\\0%s\\0%s\\0' \"$root\" \"$short_commit\"; exit 0; fi; head_remote=$(LC_ALL=C git -C \"$root\" config --get \"branch.$branch.pushRemote\" 2>/dev/null || true); if [ -z \"$head_remote\" ]; then head_remote=$(LC_ALL=C git -C \"$root\" config --get remote.pushDefault 2>/dev/null || true); fi; if [ -z \"$head_remote\" ]; then head_remote=$(LC_ALL=C git -C \"$root\" config --get \"branch.$branch.remote\" 2>/dev/null || true); fi; if [ \"$head_remote\" = '.' ]; then printf '{protocol}\\0ambiguous\\0'; exit 0; fi; if [ -z \"$head_remote\" ]; then head_remote=origin; fi; head_push=$(LC_ALL=C git -C \"$root\" config --get-all \"remote.$head_remote.pushurl\" 2>/dev/null || true); head_fetch=$(LC_ALL=C git -C \"$root\" config --get-all \"remote.$head_remote.url\" 2>/dev/null || true); origin=$(LC_ALL=C git -C \"$root\" config --get-all remote.origin.url 2>/dev/null || true); upstream=$(LC_ALL=C git -C \"$root\" config --get-all remote.upstream.url 2>/dev/null || true); parent=$(LC_ALL=C git -C \"$root\" config --get-all remote.parent.url 2>/dev/null || true); all_urls=$(for remote_name in $(LC_ALL=C git -C \"$root\" remote 2>/dev/null); do LC_ALL=C git -C \"$root\" config --get-all \"remote.$remote_name.url\" 2>/dev/null || true; done); printf '{protocol}\\0ok\\0%s\\0%s\\0%s\\0%s\\0%s\\0%s\\0%s\\0%s\\0%s\\0' \"$root\" \"$branch\" \"$head_remote\" \"$head_push\" \"$origin\" \"$upstream\" \"$parent\" \"$all_urls\" \"$head_fetch\"",
        cwd = shell_single_quote(remote_cwd),
        protocol = REMOTE_GIT_IDENTITY_PROTOCOL,
    );

    let mut argv = connection_only_ssh(ssh_argv, cwd_host);
    inject_ssh_batch_options(&mut argv);
    argv.push(remote_cmd);
    argv
}

/// Classify remote Git identity without retaining raw SSH output or stderr.
/// This keeps connection details and credential-bearing remote URLs out of UI
/// state and diagnostics.
pub fn classify_remote_git_identity(result: Result<String, String>) -> RemoteGitIdentityOutcome {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            let lower = error.to_ascii_lowercase();
            let kind = if lower.contains("permission denied (")
                || lower.contains("authentication failed")
                || lower.contains("authentication failure")
                || lower.contains("too many authentication failures")
                || lower.contains("no supported authentication methods")
                || lower.contains("host key verification failed")
            {
                RemoteGitIdentityError::Authentication
            } else {
                RemoteGitIdentityError::Transport
            };
            return RemoteGitIdentityOutcome::Error(kind);
        }
    };

    let mut fields = output.split('\0');
    if fields.next() != Some(REMOTE_GIT_IDENTITY_PROTOCOL) {
        return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
    }
    match fields.next() {
        Some("no-repository") => RemoteGitIdentityOutcome::NoRepository,
        Some("permission") => RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Permission),
        Some("root-error") => {
            RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::RootDiscovery)
        }
        Some("ambiguous") => RemoteGitIdentityOutcome::AmbiguousRemotes,
        Some("detached") => {
            let Some(checkout_root) = fields.next().filter(|value| !value.is_empty()) else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(short_commit) = fields
                .next()
                .filter(|value| value.len() == 7 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
            else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            RemoteGitIdentityOutcome::DetachedHead {
                checkout_root: checkout_root.to_string(),
                short_commit: short_commit.to_string(),
            }
        }
        Some("ok") => {
            let Some(checkout_root) = fields.next().filter(|value| !value.is_empty()) else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(branch) = fields.next().filter(|value| !value.is_empty()) else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(head_remote) = fields.next().filter(|value| !value.is_empty()) else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(head_push) = fields.next() else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(origin) = fields.next() else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(upstream) = fields.next() else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            let Some(parent) = fields.next() else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            // Older protocol fixtures ended after `parent`; treating trailing
            // absent fields as empty keeps parsing strict but backward-compatible
            // with those cached in-memory responses. `head_fetch` carries the head
            // remote's fetch URL(s), used as a fallback when the push URL is not a
            // GitHub remote (e.g. a deployment mirror).
            let all_remote_urls = fields.next().unwrap_or_default();
            let head_fetch = fields.next().unwrap_or_default();
            if fields.any(|field| !field.is_empty()) {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            }

            let head_repository = match head_repository_from_push_and_fetch(head_push, head_fetch) {
                Ok(repository) => repository,
                Err(RemoteFieldError::Unsupported) => {
                    return RemoteGitIdentityOutcome::UnsupportedProvider;
                }
                Err(RemoteFieldError::Ambiguous) => {
                    return RemoteGitIdentityOutcome::AmbiguousRemotes;
                }
            };
            let upstream = match optional_github_repository_from_remote_field(upstream) {
                Ok(repository) => repository,
                Err(RemoteFieldError::Unsupported) => {
                    return RemoteGitIdentityOutcome::UnsupportedProvider;
                }
                Err(RemoteFieldError::Ambiguous) => {
                    return RemoteGitIdentityOutcome::AmbiguousRemotes;
                }
            };
            let parent = match optional_github_repository_from_remote_field(parent) {
                Ok(repository) => repository,
                Err(RemoteFieldError::Unsupported) => {
                    return RemoteGitIdentityOutcome::UnsupportedProvider;
                }
                Err(RemoteFieldError::Ambiguous) => {
                    return RemoteGitIdentityOutcome::AmbiguousRemotes;
                }
            };
            if upstream.is_some() && parent.is_some() && upstream != parent {
                return RemoteGitIdentityOutcome::AmbiguousRemotes;
            }
            let explicit_base = upstream.or(parent);
            let base_repository = if let Some(base) = explicit_base {
                base
            } else {
                let origin = match optional_github_repository_from_remote_field(origin) {
                    Ok(repository) => repository,
                    Err(RemoteFieldError::Unsupported) => {
                        return RemoteGitIdentityOutcome::UnsupportedProvider;
                    }
                    Err(RemoteFieldError::Ambiguous) => {
                        return RemoteGitIdentityOutcome::AmbiguousRemotes;
                    }
                };
                match origin {
                    Some(origin) if origin != head_repository => origin,
                    Some(origin) => {
                        let other_github_repositories =
                            github_repositories_from_mixed_remote_field(all_remote_urls)
                                .into_iter()
                                .filter(|repository| repository != &head_repository)
                                .collect::<Vec<_>>();
                        if other_github_repositories.is_empty() {
                            origin
                        } else {
                            return RemoteGitIdentityOutcome::AmbiguousRemotes;
                        }
                    }
                    None if head_remote == "origin" => head_repository.clone(),
                    None => return RemoteGitIdentityOutcome::AmbiguousRemotes,
                }
            };
            let Some((head_owner, _)) = head_repository.split_once('/') else {
                return RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol);
            };
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                checkout_root: checkout_root.to_string(),
                branch: branch.to_string(),
                head_owner: head_owner.to_string(),
                head_repository: head_repository.clone(),
                base_repository,
            })
        }
        _ => RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol),
    }
}

/// Parse a single remote URL field (one URL per line) into the distinct GitHub
/// repositories it references.
///
/// A single remote field can legitimately carry several URLs (for example a
/// GitHub URL paired with a deployment mirror added via `git remote set-url
/// --add`). Non-GitHub URLs are filtered out rather than rejected so the mirror
/// case still resolves; the field is only reported as unsupported (`Err`) when
/// it carries URLs but none of them are GitHub.
fn github_repositories_from_remote_field(remote_urls: &str) -> Result<Vec<String>, ()> {
    let has_url = remote_urls
        .lines()
        .map(str::trim)
        .any(|url| !url.is_empty());
    let repositories = github_repositories_from_mixed_remote_field(remote_urls);
    if has_url && repositories.is_empty() {
        return Err(());
    }
    Ok(repositories)
}

/// Resolve the PR head repository from the head remote's push and fetch URL
/// fields, preferring the push destination.
///
/// `git push` sends the branch to `pushurl` when configured and otherwise to
/// `url`, so the push field wins whenever it resolves to a GitHub remote. When
/// the push field carries only non-GitHub URLs (e.g. a Fly.io or Render
/// deployment mirror) it is skipped in favour of the fetch field, so PR
/// discovery still works for repositories with a deployment push target. A field
/// resolving to more than one distinct GitHub repository is `Ambiguous`, and the
/// result is `Unsupported` only when neither field yields a GitHub remote.
fn head_repository_from_push_and_fetch(
    push_field: &str,
    fetch_field: &str,
) -> Result<String, RemoteFieldError> {
    for field in [push_field, fetch_field] {
        match github_repositories_from_remote_field(field) {
            Ok(repositories) => match repositories.as_slice() {
                [] => continue,
                [repository] => return Ok(repository.clone()),
                _ => return Err(RemoteFieldError::Ambiguous),
            },
            Err(()) => continue,
        }
    }
    Err(RemoteFieldError::Unsupported)
}

enum RemoteFieldError {
    Unsupported,
    Ambiguous,
}

fn optional_github_repository_from_remote_field(
    remote_urls: &str,
) -> Result<Option<String>, RemoteFieldError> {
    let repositories = github_repositories_from_remote_field(remote_urls)
        .map_err(|()| RemoteFieldError::Unsupported)?;
    match repositories.as_slice() {
        [] => Ok(None),
        [repository] => Ok(Some(repository.clone())),
        _ => Err(RemoteFieldError::Ambiguous),
    }
}

fn github_repositories_from_mixed_remote_field(remote_urls: &str) -> Vec<String> {
    let mut repositories = Vec::new();
    for repository in remote_urls
        .lines()
        .map(str::trim)
        .filter_map(crate::git::github_repository_slug_from_remote)
    {
        if !repositories.contains(&repository) {
            repositories.push(repository);
        }
    }
    repositories
}

/// Reduce a recorded ssh argv to a bounded, connection-only invocation.
///
/// A pane argv is observational input, not authority to replay arbitrary SSH
/// client behaviour. In particular, `-o SendEnv`, `SetEnv`, `LocalCommand`,
/// `ProxyCommand`, and `RemoteCommand`, plus config-file and forwarding flags,
/// can transfer local state or execute commands when replayed. Preserve only a
/// narrow set of connection/authentication options and the exact destination.
/// `autossh` is normalized to the sibling `ssh` binary so a one-shot probe
/// cannot turn into autossh's persistent restart loop.
fn connection_only_ssh(ssh_argv: Option<&[String]>, cwd_host: &str) -> Vec<String> {
    if let Some(argv) = ssh_argv {
        if crate::terminal::ssh_supports_remote_exec(argv) {
            if let Some(target_idx) = crate::terminal::ssh_command_target_index(argv) {
                if let Some(connection) = sanitize_recorded_ssh_connection(argv, target_idx) {
                    return connection;
                }
            }
        }
    }
    vec!["ssh".to_string(), cwd_host.to_string()]
}

fn sanitize_recorded_ssh_connection(argv: &[String], target_idx: usize) -> Option<Vec<String>> {
    let program = argv.first()?;
    let program_path = std::path::Path::new(program);
    let program_name = program_path.file_name()?.to_str()?;
    let safe_program = if program_name == "autossh" {
        program_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.join("ssh").to_string_lossy().into_owned())
            .unwrap_or_else(|| "ssh".to_string())
    } else {
        program.clone()
    };

    let mut safe = vec![safe_program];
    let mut idx = 1;
    while idx < target_idx {
        let arg = argv[idx].as_str();
        if crate::terminal::ssh_option_takes_value(arg)
            || (program_name == "autossh" && arg == "-M")
        {
            let value = argv.get(idx + 1)?;
            if safe_split_ssh_option(arg, value) {
                safe.push(arg.to_string());
                safe.push(value.clone());
            }
            idx += 2;
            continue;
        }
        if let Some(option) = arg.strip_prefix("-o") {
            if !option.is_empty() && safe_ssh_config_option(option) {
                safe.push(format!("-o{option}"));
            }
            idx += 1;
            continue;
        }
        if let Some((safe_prefix, value_flag, attached)) = clustered_ssh_value_option(arg) {
            if !safe_prefix.is_empty() {
                safe.push(format!("-{safe_prefix}"));
            }
            let safe_value_flag = "bBcIiJlmpS".contains(value_flag);
            if attached.is_empty() {
                let value = argv.get(idx + 1)?;
                if safe_value_flag && !value.chars().any(char::is_control) {
                    safe.push(format!("-{value_flag}"));
                    safe.push(value.clone());
                }
                idx += 2;
            } else {
                if safe_value_flag && !attached.chars().any(char::is_control) {
                    safe.push(format!("-{value_flag}{attached}"));
                }
                idx += 1;
            }
            continue;
        }
        if let Some(rewritten) = sanitize_attached_or_flag_ssh_option(arg) {
            safe.push(rewritten);
        }
        idx += 1;
    }
    safe.push(argv.get(target_idx)?.clone());
    Some(safe)
}

fn clustered_ssh_value_option(arg: &str) -> Option<(String, char, &str)> {
    const VALUE_FLAGS: &str = "bBcDEeFIiJLlmOoPpQRSWw";
    const SAFE_FLAGS_WITHOUT_VALUES: &str = "46Cakqvx";
    let cluster = arg
        .strip_prefix('-')?
        .strip_prefix('-')
        .is_none()
        .then_some(&arg[1..])?;
    let (byte_idx, value_flag) = cluster
        .char_indices()
        .find(|(_, flag)| VALUE_FLAGS.contains(*flag))?;
    let safe_prefix = cluster[..byte_idx]
        .chars()
        .filter(|flag| SAFE_FLAGS_WITHOUT_VALUES.contains(*flag))
        .collect();
    let attached = &cluster[byte_idx + value_flag.len_utf8()..];
    Some((safe_prefix, value_flag, attached))
}

fn safe_split_ssh_option(option: &str, value: &str) -> bool {
    if value.chars().any(char::is_control) {
        return false;
    }
    match option {
        "-b" | "-B" | "-c" | "-I" | "-i" | "-J" | "-l" | "-m" | "-p" | "-S" => true,
        "-o" => safe_ssh_config_option(value),
        _ => false,
    }
}

fn sanitize_attached_or_flag_ssh_option(arg: &str) -> Option<String> {
    let flags = arg.strip_prefix('-')?;
    if flags.is_empty() || flags.starts_with('-') {
        return None;
    }
    // Harmless connection modifiers only. Credential delegation, forwarding,
    // backgrounding, tty, subsystem, and command/query flags are omitted.
    const SAFE_FLAGS_WITHOUT_VALUES: &str = "46Cakqvx";
    let retained: String = flags
        .chars()
        .filter(|flag| SAFE_FLAGS_WITHOUT_VALUES.contains(*flag))
        .collect();
    (!retained.is_empty()).then(|| format!("-{retained}"))
}

fn safe_ssh_config_option(option: &str) -> bool {
    let parsed = option.split_once('=').or_else(|| {
        let separator = option.find(char::is_whitespace)?;
        Some((&option[..separator], option[separator..].trim_start()))
    });
    let Some((name, value)) = parsed else {
        return false;
    };
    if name.is_empty() || value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "addressfamily"
            | "bindaddress"
            | "bindinterface"
            | "casignaturealgorithms"
            | "certificatefile"
            | "checkhostip"
            | "ciphers"
            | "compression"
            | "controlmaster"
            | "controlpath"
            | "controlpersist"
            | "fingerprinthash"
            | "globalknownhostsfile"
            | "hashknownhosts"
            | "hostbasedacceptedalgorithms"
            | "hostbasedauthentication"
            | "hostkeyalgorithms"
            | "hostkeyalias"
            | "hostname"
            | "identitiesonly"
            | "identityagent"
            | "identityfile"
            | "ipqos"
            | "kbdinteractiveauthentication"
            | "kexalgorithms"
            | "loglevel"
            | "macs"
            | "nohostauthenticationforlocalhost"
            | "numberofpasswordprompts"
            | "passwordauthentication"
            | "pkcs11provider"
            | "port"
            | "preferredauthentications"
            | "proxyjump"
            | "pubkeyacceptedalgorithms"
            | "pubkeyauthentication"
            | "rekeylimit"
            | "requiredrsasize"
            | "securitykeyprovider"
            | "serveralivecountmax"
            | "serveraliveinterval"
            | "tcpkeepalive"
            | "updatehostkeys"
            | "user"
            | "userknownhostsfile"
            | "verifyhostkeydns"
    )
}

/// Insert fail-closed probe options before recorded arguments. OpenSSH uses the
/// first value obtained for scalar options, so these also override unsafe
/// forwarding/delegation settings inherited from the user's normal config.
fn inject_ssh_batch_options(argv: &mut Vec<String>) {
    let is_ssh = argv
        .first()
        .map(|program| {
            let name = program.rsplit('/').next().unwrap_or(program);
            name == "ssh" || name == "autossh"
        })
        .unwrap_or(false);
    if !is_ssh {
        return;
    }
    let extras = [
        "-T",
        "-oBatchMode=yes",
        "-oConnectTimeout=5",
        "-oConnectionAttempts=1",
        "-oClearAllForwardings=yes",
        "-oForwardAgent=no",
        "-oForwardX11=no",
        "-oForwardX11Trusted=no",
        "-oGSSAPIDelegateCredentials=no",
        "-oPermitLocalCommand=no",
        "-oRemoteCommand=none",
        "-oRequestTTY=no",
    ];
    for (offset, extra) in extras.into_iter().enumerate() {
        argv.insert(1 + offset, extra.to_string());
    }
}

/// Quote a string for safe inclusion in a remote POSIX shell command.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn effort_weight(effort: Option<&str>) -> usize {
    match effort.unwrap_or_default().trim() {
        "quick" => 1,
        "medium" => 2,
        "large" => 3,
        _ => 1,
    }
}

fn normalize_task_status(status: &str) -> String {
    status.trim().to_ascii_lowercase()
}

fn normalize_pr_state(state: &str) -> String {
    state.trim().to_ascii_lowercase()
}

fn normalize_review_decision(decision: &str) -> String {
    decision.trim().replace('_', " ").to_ascii_lowercase()
}

fn normalized_id_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn is_done_status(status: &str) -> bool {
    matches!(status.trim(), "done" | "completed" | "closed")
}

fn is_in_progress_status(status: &str) -> bool {
    matches!(
        status.trim(),
        "in-progress" | "in_progress" | "doing" | "active" | "started"
    )
}

fn task_status_rank(task: &PlanTaskEntry) -> u8 {
    if task.is_in_progress() {
        0
    } else if !task.is_done() && !task.is_blocked() {
        1
    } else if task.is_blocked() {
        2
    } else if task.is_done() {
        3
    } else {
        4
    }
}

fn task_priority_rank(priority: Option<&str>) -> u8 {
    match priority
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "p0" => 0,
        "p1" => 1,
        "p2" => 2,
        "p3" => 3,
        _ => 4,
    }
}

fn pr_status_rank(pr: &BranchPullRequestEntry) -> u8 {
    if pr.state == "open" && !pr.is_draft {
        return 0;
    }
    if pr.is_draft {
        return 1;
    }
    match pr.state.as_str() {
        "merged" => 2,
        "closed" => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        branch_pull_requests_command, classify_remote_git_identity, classify_remote_tasks_fetch,
        compute_parallelization, parse_pull_request_correlation_metadata,
        remote_git_identity_command, remote_tasks_fetch_command, BranchPullRequestsData,
        PlanTaskEntry, PlanTaskProjectionCache, PlanTasksData, PullRequestChecks,
        PullRequestCorrelation, RemoteGitIdentity, RemoteGitIdentityError,
        RemoteGitIdentityOutcome, RemoteTasksError, RemoteTasksOutcome, TrackingData,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Build a `PlanTaskEntry` with the dependency wiring the parallelization
    /// tests exercise. `unresolved` is the count the panel uses for blocked-ness.
    fn plan_task(id: &str, status: &str, depends_on: &[&str], unresolved: usize) -> PlanTaskEntry {
        let depends_on_ids: Vec<String> = depends_on.iter().map(|d| d.to_string()).collect();
        PlanTaskEntry {
            id: id.to_string(),
            title: format!("Task {id}"),
            status: status.to_string(),
            priority: None,
            blocked_by: 0,
            depends_on: depends_on_ids.len(),
            unresolved_depends_on: unresolved,
            depends_on_ids,
            assigned_model: None,
        }
    }

    fn temp_repo() -> PathBuf {
        // Tests run in parallel, and `SystemTime::now()` is not guaranteed to be
        // unique across threads (the clock can be coarser than nanoseconds, so two
        // calls can read the same instant). A timestamp-only name lets two temp
        // repos collide onto the same directory, so one test can delete or
        // overwrite the `.plan/tasks.json` another just wrote. Add a monotonic
        // per-process counter (plus the pid) so every temp repo is distinct.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "taarof-tracking-test-{}-{}-{}",
            std::process::id(),
            ts.as_nanos(),
            unique
        ));
        fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn write_features(repo_root: &Path, json: &str) {
        let plan_dir = repo_root.join(".plan");
        fs::create_dir_all(&plan_dir).expect("create .plan");
        fs::write(plan_dir.join("features.json"), json).expect("write features.json");
    }

    fn write_tasks(repo_root: &Path, json: &str) {
        let plan_dir = repo_root.join(".plan");
        fs::create_dir_all(&plan_dir).expect("create .plan");
        fs::write(plan_dir.join("tasks.json"), json).expect("write tasks.json");
    }

    #[test]
    fn load_valid_json_returns_counts() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F1","title":"A","status":"done"},
                {"id":"F2","title":"B","status":"queued"},
                {"id":"F3","title":"C","status":"in-progress"}
            ]}"#,
        );

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.total, 3);
        assert_eq!(data.done, 1);
        assert_eq!(data.queued, 1);
        assert_eq!(data.in_progress, 1);
    }

    #[test]
    fn load_missing_file_returns_none() {
        let repo = temp_repo();
        assert!(TrackingData::load(&repo).is_none());
    }

    #[test]
    fn load_malformed_json_returns_none() {
        let repo = temp_repo();
        write_features(&repo, "{not-json");
        assert!(TrackingData::load(&repo).is_none());
    }

    #[test]
    fn plan_tasks_remote_from_json_str_parses_same_as_local_file() {
        let repo = temp_repo();
        let json = r#"{"tasks":[
            {"id":"T-1","title":"One","status":"todo"},
            {"id":"T-2","title":"Two","status":"done"}
        ]}"#;
        write_tasks(&repo, json);

        let from_file = PlanTasksData::load(&repo).expect("local file load");
        let from_str = PlanTasksData::from_json_str(json).expect("string parse");

        // Remote `cat` output and the local file must parse to identical data.
        assert_eq!(from_file, from_str);
        assert_eq!(from_str.total, 2);
        assert_eq!(from_str.done, 1);
    }

    #[test]
    fn branch_pull_requests_parse_gh_json_and_sort_active_first() {
        let json = r#"[
  {
    "number": 42,
    "title": "Draft branch work",
    "state": "OPEN",
    "isDraft": true,
    "reviewDecision": null,
    "url": "https://github.com/Example-Org/Example-Repo/pull/42",
    "headRefName": "feature/task-panel",
    "headRepositoryOwner": {"login":"Example-Org"},
    "baseRefName": "main",
    "updatedAt": "2026-06-18T15:00:00Z"
  },
  {
    "number": 41,
    "title": "Ready branch work",
    "state": "OPEN",
    "isDraft": false,
    "reviewDecision": "APPROVED",
    "url": "https://github.com/Example-Org/Example-Repo/pull/41",
    "headRefName": "feature/task-panel",
    "headRepositoryOwner": {"login":"Example-Org"},
    "baseRefName": "main",
    "updatedAt": "2026-06-19T15:00:00Z"
  },
  {
    "number": 40,
    "title": "Old branch work",
    "state": "MERGED",
    "isDraft": false,
    "reviewDecision": null,
    "url": "https://github.com/Example-Org/Example-Repo/pull/40",
    "headRefName": "feature/task-panel",
    "headRepositoryOwner": {"login":"Example-Org"},
    "baseRefName": "main",
    "updatedAt": "2026-06-17T15:00:00Z"
  }
]"#;

        let data = BranchPullRequestsData::from_gh_json_str(json).expect("gh JSON should parse");

        assert_eq!(data.total, 3);
        assert_eq!(data.open, 2);
        assert_eq!(data.draft, 1);
        assert_eq!(data.merged, 1);
        assert_eq!(data.closed, 0);
        assert_eq!(
            data.pull_requests
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>(),
            vec![41, 42, 40]
        );
        assert_eq!(data.pull_requests[0].status_label(), "open · approved");
        assert_eq!(data.pull_requests[1].status_label(), "draft");
    }

    #[test]
    fn branch_pull_requests_command_scopes_to_head_branch() {
        assert_eq!(
            branch_pull_requests_command(
                "example-org/example-repo",
                "feature/task-panel",
            ),
            vec![
                "gh",
                "pr",
                "list",
                "--state",
                "all",
                "--repo",
                "example-org/example-repo",
                "--head",
                "feature/task-panel",
                "--json",
                "number,title,state,isDraft,url,reviewDecision,updatedAt,headRefName,headRepositoryOwner,baseRefName,body,statusCheckRollup",
                "--limit",
                "1000",
            ]
        );
    }

    #[test]
    fn branch_pull_requests_filter_enforces_fork_head_owner() {
        let json = r#"[
          {"number":1,"title":"fork","state":"OPEN","headRefName":"agent/work","headRepositoryOwner":{"login":"fork-owner"},"baseRefName":"main"},
          {"number":2,"title":"collision","state":"OPEN","headRefName":"agent/work","headRepositoryOwner":{"login":"other-owner"},"baseRefName":"main"},
          {"number":3,"title":"other branch","state":"MERGED","headRefName":"other","headRepositoryOwner":{"login":"fork-owner"},"baseRefName":"main"},
          {"number":4,"title":"deleted fork","state":"OPEN","headRefName":"agent/work","headRepositoryOwner":null,"baseRefName":"main"}
        ]"#;
        let data = BranchPullRequestsData::from_gh_json_str(json)
            .unwrap()
            .into_exact_head("FORK-OWNER", "agent/work");
        assert_eq!(data.total, 1);
        assert_eq!(data.open, 1);
        assert_eq!(data.merged, 0);
        assert_eq!(data.pull_requests[0].number, 1);
    }

    #[test]
    fn remote_git_identity_parses_github_fork_and_upstream() {
        let output = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/legacy-app\0agent/EXAMPLE-130\0",
            "origin\0",
            "git@github.com:fork-owner/legacy-app.git\0",
            "git@github.com:fork-owner/legacy-app.git\0",
            "https://github.com/Example-Org/Example-Repo.git\0\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(output.to_string())),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                checkout_root: "/srv/legacy-app".into(),
                branch: "agent/EXAMPLE-130".into(),
                head_repository: "fork-owner/legacy-app".into(),
                base_repository: "example-org/example-repo".into(),
                head_owner: "fork-owner".into(),
            })
        );
    }

    #[test]
    fn remote_git_identity_distinguishes_detached_gitlab_and_ambiguous_remotes() {
        assert_eq!(
            classify_remote_git_identity(Ok(
                "TAAROF_REMOTE_GIT_IDENTITY_V1\0detached\0/srv/repo\0abcdef1\0".into()
            )),
            RemoteGitIdentityOutcome::DetachedHead {
                checkout_root: "/srv/repo".into(),
                short_commit: "abcdef1".into(),
            }
        );
        assert_eq!(
            classify_remote_git_identity(Ok(concat!(
                "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
                "origin\0git@gitlab.com:owner/repo.git\0",
                "git@gitlab.com:owner/repo.git\0\0\0"
            )
            .into())),
            RemoteGitIdentityOutcome::UnsupportedProvider
        );
        assert_eq!(
            classify_remote_git_identity(Ok(concat!(
                "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
                "origin\0",
                "git@github.com:fork/repo.git\0",
                "git@github.com:fork/repo.git\0",
                "git@gitlab.com:owner/repo.git\0\0"
            )
            .into())),
            RemoteGitIdentityOutcome::UnsupportedProvider
        );
        assert_eq!(
            classify_remote_git_identity(Ok(concat!(
                "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
                "origin\0",
                "git@github.com:fork/repo.git\0",
                "git@github.com:fork/repo.git\0",
                "git@github.com:owner/repo.git\0",
                "git@github.com:other/repo.git\0"
            )
            .into())),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );
    }

    #[test]
    fn remote_git_identity_filters_deployment_mirrors_from_head_and_base() {
        // `git remote set-url --add` can pair a GitHub URL with a deployment
        // mirror (Fly.io, Render, etc.) on the same remote. The mirror must be
        // filtered out rather than blocking discovery as `UnsupportedProvider`.
        let output = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0feature\0",
            "origin\0",
            "git@github.com:fork/repo.git\nhttps://deploy.example.com/app.git\0",
            "git@github.com:fork/repo.git\0",
            "https://github.com/owner/repo.git\nhttps://render.example.com/x.git\0",
            "\0\0\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(output.into())),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                checkout_root: "/srv/repo".into(),
                branch: "feature".into(),
                head_repository: "fork/repo".into(),
                base_repository: "owner/repo".into(),
                head_owner: "fork".into(),
            })
        );
    }

    #[test]
    fn remote_git_identity_falls_back_to_fetch_url_when_push_is_non_github() {
        // A non-GitHub deployment push URL must not shadow a GitHub fetch URL on
        // the same remote; discovery falls back from `pushurl` to `url`.
        let output = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0feature\0",
            "origin\0",
            "https://deploy.example.com/app.git\0",
            "git@github.com:owner/repo.git\0",
            "\0\0\0",
            "git@github.com:owner/repo.git\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(output.into())),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                checkout_root: "/srv/repo".into(),
                branch: "feature".into(),
                head_repository: "owner/repo".into(),
                base_repository: "owner/repo".into(),
                head_owner: "owner".into(),
            })
        );
    }

    #[test]
    fn remote_git_identity_prefers_push_url_over_fetch_url() {
        // `git push` targets `pushurl` when set, so a GitHub push URL wins over a
        // distinct GitHub fetch URL without being treated as ambiguous.
        let output = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0feature\0",
            "origin\0",
            "git@github.com:push-owner/repo.git\0",
            "git@github.com:base-owner/repo.git\0",
            "git@github.com:base-owner/repo.git\0",
            "\0\0",
            "git@github.com:fetch-owner/repo.git\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(output.into())),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                checkout_root: "/srv/repo".into(),
                branch: "feature".into(),
                head_repository: "push-owner/repo".into(),
                base_repository: "base-owner/repo".into(),
                head_owner: "push-owner".into(),
            })
        );
    }

    #[test]
    fn remote_git_identity_probe_reuses_connection_without_github_credentials() {
        let recorded = vec![
            "ssh".to_string(),
            "-i".to_string(),
            "/keys/dev".to_string(),
            "-p".to_string(),
            "2222".to_string(),
            "-t".to_string(),
            "dev@remote".to_string(),
            "cd /old && exec zsh".to_string(),
        ];
        let command = remote_git_identity_command(Some(&recorded), "remote", "/srv/repo");
        assert_eq!(command[0], "ssh");
        assert!(command.windows(2).any(|pair| pair == ["-i", "/keys/dev"]));
        assert!(command.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert!(command.contains(&"dev@remote".to_string()));
        assert!(!command.contains(&"-t".to_string()));
        let remote_script = command.last().unwrap();
        assert!(remote_script.contains("symbolic-ref --quiet --short HEAD"));
        assert!(!remote_script.contains("gh "));
        assert!(!remote_script.contains("GH_TOKEN"));
        assert!(!remote_script.contains("GITHUB_TOKEN"));
    }

    #[test]
    fn remote_git_identity_probe_resolves_nested_checkout_with_real_git() {
        let root = temp_repo();
        let nested = root.join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        };
        run(&["init", "-b", "agent/EXAMPLE-130"]);
        run(&[
            "remote",
            "add",
            "origin",
            "git@github.com:fork-owner/legacy-app.git",
        ]);
        run(&[
            "remote",
            "add",
            "upstream",
            "https://github.com/Example-Org/Example-Repo.git",
        ]);

        let command = remote_git_identity_command(None, "unused", nested.to_str().unwrap());
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        let outcome = classify_remote_git_identity(Ok(
            String::from_utf8(output.stdout).expect("probe output is utf8")
        ));
        assert!(matches!(
            outcome,
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                branch,
                head_owner,
                base_repository,
                ..
            }) if branch == "agent/EXAMPLE-130"
                && head_owner == "fork-owner"
                && base_repository == "example-org/example-repo"
        ));

        // A common fork layout keeps the canonical repository as `origin` and
        // pushes the active branch through another remote. The branch remote,
        // not the conventional remote name, owns the PR head in that layout.
        run(&[
            "remote",
            "set-url",
            "origin",
            "https://github.com/Example-Org/Example-Repo.git",
        ]);
        run(&[
            "remote",
            "add",
            "fork",
            "git@github.com:fork-owner/legacy-app.git",
        ]);
        run(&["config", "branch.agent/EXAMPLE-130.remote", "fork"]);
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(matches!(
            classify_remote_git_identity(Ok(
                String::from_utf8(output.stdout).expect("probe output is utf8")
            )),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                head_repository,
                base_repository,
                head_owner,
                ..
            }) if head_repository == "fork-owner/legacy-app"
                && base_repository == "example-org/example-repo"
                && head_owner == "fork-owner"
        ));

        // Git push prefers pushurl over url. The PR head owner must follow the
        // actual push destination, not the fetch URL recorded for the remote.
        run(&["config", "branch.agent/EXAMPLE-130.pushRemote", "fork"]);
        run(&[
            "config",
            "remote.fork.pushurl",
            "git@github.com:push-owner/legacy-app.git",
        ]);
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .output()
            .unwrap();
        assert!(matches!(
            classify_remote_git_identity(Ok(
                String::from_utf8(output.stdout).expect("probe output is utf8")
            )),
            RemoteGitIdentityOutcome::GitHub(RemoteGitIdentity {
                head_repository,
                base_repository,
                ..
            }) if head_repository == "push-owner/legacy-app"
                && base_repository == "example-org/example-repo"
        ));

        // Multiple distinct push destinations cannot identify one exact PR
        // head, and the special local-repository remote (`.`) is not GitHub.
        run(&[
            "config",
            "--add",
            "remote.fork.pushurl",
            "git@github.com:other-owner/legacy-app.git",
        ]);
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .output()
            .unwrap();
        assert_eq!(
            classify_remote_git_identity(Ok(
                String::from_utf8(output.stdout).expect("probe output is utf8")
            )),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );
        run(&["config", "--unset-all", "remote.fork.pushurl"]);
        run(&["config", "branch.agent/EXAMPLE-130.pushRemote", "."]);
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .output()
            .unwrap();
        assert_eq!(
            classify_remote_git_identity(Ok(
                String::from_utf8(output.stdout).expect("probe output is utf8")
            )),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_git_identity_refuses_to_guess_base_from_unlabelled_github_remotes() {
        let output = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
            "origin\0git@github.com:owner/repo.git\0",
            "git@github.com:owner/repo.git\0\0\0",
            "git@github.com:owner/repo.git\n",
            "git@github.com:other/canonical.git\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(output.into())),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );

        let missing_origin = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
            "fork\0git@github.com:fork/repo.git\0\0\0\0",
            "git@github.com:fork/repo.git\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(missing_origin.into())),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );

        let multiple_origin_urls = concat!(
            "TAAROF_REMOTE_GIT_IDENTITY_V1\0ok\0/srv/repo\0main\0",
            "fork\0git@github.com:fork/repo.git\0",
            "git@github.com:owner/one.git\ngit@github.com:owner/two.git\0\0\0\0"
        );
        assert_eq!(
            classify_remote_git_identity(Ok(multiple_origin_urls.into())),
            RemoteGitIdentityOutcome::AmbiguousRemotes
        );
    }

    #[test]
    fn remote_git_identity_probe_shell_quotes_untrusted_remote_cwd() {
        let root = temp_repo();
        let sentinel = root.join("injected");
        let hostile = root.join("nested'; touch injected; echo '");
        fs::create_dir_all(&hostile).unwrap();
        let output = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:owner/repo.git"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());

        let command = remote_git_identity_command(None, "unused", &hostile.to_string_lossy());
        let output = Command::new("sh")
            .args(["-c", command.last().unwrap()])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            !sentinel.exists(),
            "remote cwd escaped its shell-quoted slot"
        );
        assert!(matches!(
            classify_remote_git_identity(Ok(
                String::from_utf8(output.stdout).expect("probe output is utf8")
            )),
            RemoteGitIdentityOutcome::GitHub(_)
        ));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(sentinel);
    }

    #[test]
    fn remote_git_identity_failures_discard_secret_bearing_ssh_output() {
        let secret = "permission denied (publickey): token=super-secret";
        let outcome = classify_remote_git_identity(Err(secret.into()));
        assert_eq!(
            outcome,
            RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Authentication)
        );
        assert!(!format!("{outcome:?}").contains("super-secret"));
    }

    #[test]
    fn structured_pr_metadata_and_checks_are_verified_against_github_identity() {
        let json = r#"[{
          "number": 321,
          "title": "Correlation",
          "state": "OPEN",
          "isDraft": false,
          "reviewDecision": "APPROVED",
          "url": "https://github.com/Example-Org/Example-Repo/pull/321",
          "headRefName": "agent/EXAMPLE-113-task-pr-correlation",
          "headRepositoryOwner": {"login":"Example-Org"},
          "baseRefName": "main",
          "updatedAt": "2026-07-14T12:00:00Z",
          "body": "<!-- taarof-work: {\"schema\":\"taarof.pr-work.v1\",\"task_id\":\"EXAMPLE-113\",\"repo\":\"example-org/example-repo\",\"branch\":\"agent/EXAMPLE-113-task-pr-correlation\",\"dispatch_source\":\"taarof\"} -->",
          "statusCheckRollup": [
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"StatusContext","state":"SUCCESS"}
          ]
        }]"#;

        let data = BranchPullRequestsData::from_gh_json_str(json).unwrap();
        let pr = &data.pull_requests[0];
        assert_eq!(pr.checks, PullRequestChecks::Ready);
        assert_eq!(pr.correlation.as_ref().unwrap().task_id, "EXAMPLE-113");
        assert_eq!(
            pr.correlation.as_ref().unwrap().repo,
            "example-org/example-repo"
        );
    }

    #[test]
    fn correlation_rejects_duplicate_or_mismatched_markers_and_never_reads_prose() {
        let marker = "<!-- taarof-work: {\"schema\":\"taarof.pr-work.v1\",\"task_id\":\"EXAMPLE-113\",\"repo\":\"example-org/example-repo\",\"branch\":\"agent/one\",\"dispatch_source\":\"taarof\"} -->";
        let duplicate = format!("{marker}\n{marker}");
        assert!(parse_pull_request_correlation_metadata(
            &duplicate,
            "https://github.com/Example-Org/Example-Repo/pull/1",
            "example-org",
            "agent/one"
        )
        .is_none());
        assert!(parse_pull_request_correlation_metadata(
            "Implement EXAMPLE-113 and closes #321",
            "https://github.com/Example-Org/Example-Repo/pull/1",
            "example-org",
            "agent/one"
        )
        .is_none());
        assert!(parse_pull_request_correlation_metadata(
            marker,
            "https://github.com/Other/repo/pull/1",
            "example-org",
            "agent/one"
        )
        .is_none());
        let non_normalized = marker.replace("example-org/example-repo", "Example-Org/Example-Repo");
        assert!(parse_pull_request_correlation_metadata(
            &non_normalized,
            "https://github.com/Example-Org/Example-Repo/pull/1",
            "example-org",
            "agent/one"
        )
        .is_none());
    }

    #[test]
    fn correlation_prefers_unique_explicit_metadata_and_leaves_ambiguity_unlinked() {
        fn pr(number: u64, task: Option<&str>) -> super::BranchPullRequestEntry {
            super::BranchPullRequestEntry {
                number,
                title: format!("PR {number}"),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                url: Some(format!("https://github.com/owner/repo/pull/{number}")),
                head_ref_name: "agent/work".into(),
                head_repository_owner: "owner".into(),
                base_ref_name: "main".into(),
                updated_at: None,
                checks: PullRequestChecks::Pending,
                correlation: task.map(|task_id| super::PullRequestCorrelationMetadata {
                    task_id: task_id.into(),
                    repo: "owner/repo".into(),
                    branch: "agent/work".into(),
                    dispatch_source: "taarof".into(),
                }),
                correlation_marker_present: task.is_some(),
            }
        }
        fn data(pull_requests: Vec<super::BranchPullRequestEntry>) -> BranchPullRequestsData {
            BranchPullRequestsData {
                total: pull_requests.len(),
                open: pull_requests.len(),
                draft: 0,
                merged: 0,
                closed: 0,
                pull_requests,
                query_complete: true,
            }
        }
        let candidates = data(vec![pr(1, Some("OTHER")), pr(2, Some("EXAMPLE-113"))]);
        assert!(matches!(
            super::correlate_pull_request("EXAMPLE-113", "owner/repo", "owner", "agent/work", &candidates),
            PullRequestCorrelation::Linked(pr) if pr.number == 2
        ));
        let ambiguous = data(vec![pr(2, Some("EXAMPLE-113")), pr(3, Some("EXAMPLE-113"))]);
        assert_eq!(
            super::correlate_pull_request(
                "EXAMPLE-113",
                "owner/repo",
                "owner",
                "agent/work",
                &ambiguous
            ),
            PullRequestCorrelation::Ambiguous
        );
        let unmarked = data(vec![pr(4, None)]);
        assert_eq!(
            super::correlate_pull_request(
                "EXAMPLE-113",
                "owner/repo",
                "owner",
                "agent/work",
                &unmarked
            ),
            PullRequestCorrelation::Unlinked
        );
        let conflicting = data(vec![pr(5, Some("OTHER"))]);
        assert_eq!(
            super::correlate_pull_request(
                "EXAMPLE-113",
                "owner/repo",
                "owner",
                "agent/work",
                &conflicting
            ),
            PullRequestCorrelation::Unlinked
        );
    }

    #[test]
    fn malformed_extra_marker_blocks_branch_fallback() {
        let valid = "<!-- taarof-work: {\"schema\":\"taarof.pr-work.v1\",\"task_id\":\"EXAMPLE-113\",\"repo\":\"owner/repo\",\"branch\":\"agent/work\",\"dispatch_source\":\"taarof\"} -->";
        let body = format!("{valid}\n<!-- taarof-work: malformed -->");
        let json = serde_json::json!([{
            "number": 7,
            "title": "Ambiguous marker",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/7",
            "headRefName": "agent/work",
            "headRepositoryOwner": {"login": "owner"},
            "baseRefName": "main",
            "body": body,
        }]);
        let data = BranchPullRequestsData::from_gh_json_str(&json.to_string()).unwrap();
        assert!(data.pull_requests[0].correlation.is_none());
        assert!(data.pull_requests[0].correlation_marker_present);
        assert_eq!(
            super::correlate_pull_request(
                "EXAMPLE-113",
                "owner/repo",
                "owner",
                "agent/work",
                &data
            ),
            PullRequestCorrelation::Unlinked
        );
    }

    #[test]
    fn fork_pr_correlates_base_repository_with_head_owner_and_branch() {
        let json = serde_json::json!([{
            "number": 8,
            "title": "Fork contribution",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/upstream/project/pull/8",
            "headRefName": "agent/work",
            "headRepositoryOwner": {"login": "fork-user"},
            "baseRefName": "main",
            "body": "<!-- taarof-work: {\"schema\":\"taarof.pr-work.v1\",\"task_id\":\"TASK-8\",\"repo\":\"upstream/project\",\"branch\":\"agent/work\",\"dispatch_source\":\"taarof\"} -->",
        }]);
        let data = BranchPullRequestsData::from_gh_json_str(&json.to_string()).unwrap();
        assert!(matches!(
            super::correlate_pull_request(
                "TASK-8",
                "upstream/project",
                "fork-user",
                "agent/work",
                &data,
            ),
            PullRequestCorrelation::Linked(pr) if pr.number == 8
        ));
        assert_eq!(
            super::correlate_pull_request(
                "TASK-8",
                "upstream/project",
                "upstream",
                "agent/work",
                &data,
            ),
            PullRequestCorrelation::Unlinked
        );
    }

    #[test]
    fn unmarked_fork_prs_never_create_task_links() {
        let json = serde_json::json!([
            {
                "number": 8,
                "title": "Expected fork",
                "state": "OPEN",
                "isDraft": false,
                "url": "https://github.com/upstream/project/pull/8",
                "headRefName": "agent/work",
                "headRepositoryOwner": {"login": "expected-fork"},
                "baseRefName": "main"
            },
            {
                "number": 9,
                "title": "Different fork, same branch",
                "state": "OPEN",
                "isDraft": false,
                "url": "https://github.com/upstream/project/pull/9",
                "headRefName": "agent/work",
                "headRepositoryOwner": {"login": "other-fork"},
                "baseRefName": "main"
            }
        ]);
        let data = BranchPullRequestsData::from_gh_json_str(&json.to_string()).unwrap();
        assert_eq!(
            super::correlate_pull_request(
                "TASK-8",
                "upstream/project",
                "expected-fork",
                "agent/work",
                &data,
            ),
            PullRequestCorrelation::Unlinked
        );
    }

    #[test]
    fn incomplete_limit_bound_query_never_correlates() {
        let rows = (1..=super::PULL_REQUEST_QUERY_LIMIT)
            .map(|number| {
                serde_json::json!({
                    "number": number,
                    "title": format!("PR {number}"),
                    "state": "OPEN",
                    "isDraft": false,
                    "url": format!("https://github.com/owner/repo/pull/{number}"),
                    "headRefName": "agent/work",
                    "headRepositoryOwner": {"login": "owner"},
                    "baseRefName": "main",
                })
            })
            .collect::<Vec<_>>();
        let data = BranchPullRequestsData::from_gh_json_str(
            &serde_json::to_string(&rows).expect("serialize limit fixture"),
        )
        .unwrap();
        assert!(!data.query_complete);
        assert_eq!(
            super::correlate_pull_request("TASK", "owner/repo", "owner", "agent/work", &data),
            PullRequestCorrelation::Incomplete
        );
    }

    #[test]
    fn exhaustive_unmarked_query_remains_unlinked() {
        let pull_requests = (1..=21)
            .map(|number| super::BranchPullRequestEntry {
                number,
                title: format!("PR {number}"),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                url: Some(format!("https://github.com/owner/repo/pull/{number}")),
                head_ref_name: "agent/work".into(),
                head_repository_owner: "owner".into(),
                base_ref_name: "main".into(),
                updated_at: None,
                checks: PullRequestChecks::None,
                correlation: None,
                correlation_marker_present: false,
            })
            .collect::<Vec<_>>();
        let data = BranchPullRequestsData {
            total: pull_requests.len(),
            open: pull_requests.len(),
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests,
            query_complete: true,
        };
        assert_eq!(
            super::correlate_pull_request("TASK", "owner/repo", "owner", "agent/work", &data),
            PullRequestCorrelation::Unlinked
        );
    }

    #[test]
    fn marker_requires_exact_github_pr_url() {
        let marker = "<!-- taarof-work: {\"schema\":\"taarof.pr-work.v1\",\"task_id\":\"TASK\",\"repo\":\"owner/repo\",\"branch\":\"agent/work\",\"dispatch_source\":\"taarof\"} -->";
        for unsafe_url in [
            "https://github.com/owner/repo/pull/1/files",
            "https://github.com/owner/repo/pull/1?diff=split",
            "https://github.com/owner/repo/pull/0",
        ] {
            assert!(parse_pull_request_correlation_metadata(
                marker,
                unsafe_url,
                "owner",
                "agent/work",
            )
            .is_none());
        }
    }

    #[test]
    fn plan_tasks_remote_target_prefers_ssh_command_over_cwd_host() {
        let argv = vec!["ssh".to_string(), "devbox".to_string()];
        let cmd = remote_tasks_fetch_command(Some(&argv), "otherhost", "/tmp/user/proj");

        // The recorded ssh target wins over the OSC 7 host.
        assert!(cmd.contains(&"devbox".to_string()));
        assert!(!cmd.iter().any(|arg| arg == "otherhost"));
        // It resolves the checkout root from the nested cwd before reading the plan.
        assert!(cmd
            .last()
            .unwrap()
            .contains("cwd='/tmp/user/proj'; root=$(LC_ALL=C git -C \"$cwd\""));
        assert!(!cmd
            .last()
            .unwrap()
            .contains("/tmp/user/proj/.plan/tasks.json"));
        assert!(cmd.iter().any(|arg| arg == "-oBatchMode=yes"));
        assert!(cmd.iter().any(|arg| arg == "-oConnectTimeout=5"));

        // Without a recorded argv, fall back to `ssh <cwd_host>`.
        let fallback = remote_tasks_fetch_command(None, "otherhost", "/srv/app/");
        assert_eq!(fallback.first().unwrap(), "ssh");
        assert!(fallback.contains(&"otherhost".to_string()));
        assert!(fallback
            .last()
            .unwrap()
            .contains("cwd='/srv/app/'; root=$(LC_ALL=C git -C \"$cwd\""));
    }

    fn run_remote_tasks_probe_script(cwd: &Path) -> RemoteTasksOutcome {
        let command = remote_tasks_fetch_command(None, "unused-host", &cwd.to_string_lossy());
        let script = command.last().expect("remote probe script");
        let output = Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run remote probe script locally");
        assert!(output.status.success());
        classify_remote_tasks_fetch(Ok(
            String::from_utf8(output.stdout).expect("utf8 probe output")
        ))
    }

    #[test]
    fn remote_tasks_probe_resolves_nested_checkout_and_quiet_negative_sources() {
        let checkout = temp_repo();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&checkout)
            .status()
            .expect("git init")
            .success()
            .then_some(())
            .expect("git init succeeds");
        let nested = checkout.join("src/deep");
        fs::create_dir_all(&nested).expect("nested checkout cwd");
        fs::create_dir_all(checkout.join(".plan")).expect("plan directory");
        fs::write(checkout.join(".plan/tasks.json"), r#"{"tasks":[]}"#)
            .expect("remote tasks fixture");

        assert_eq!(
            run_remote_tasks_probe_script(&nested),
            RemoteTasksOutcome::Loaded {
                checkout_root: checkout.to_string_lossy().into_owned(),
                data: PlanTasksData::from_json_str(r#"{"tasks":[]}"#).unwrap(),
            }
        );

        fs::remove_file(checkout.join(".plan/tasks.json")).expect("remove plan fixture");
        assert_eq!(
            run_remote_tasks_probe_script(&nested),
            RemoteTasksOutcome::NoTaskSource {
                checkout_root: Some(checkout.to_string_lossy().into_owned()),
            }
        );

        let non_repo = temp_repo();
        assert_eq!(
            run_remote_tasks_probe_script(&non_repo),
            RemoteTasksOutcome::NoTaskSource {
                checkout_root: None,
            }
        );

        fs::remove_dir_all(checkout).expect("remove checkout fixture");
        fs::remove_dir_all(non_repo).expect("remove non-repo fixture");
    }

    #[test]
    fn plan_tasks_remote_fetch_strips_interactive_remote_command_and_tty() {
        // Real panes record the full ssh cmdline, including a tty flag and an
        // interactive remote command. The fetch must reduce that to a clean
        // connection + cat, not append cat onto `exec bash`.
        let argv = vec![
            "ssh".to_string(),
            "-ttt".to_string(),
            "control".to_string(),
            "cd /srv/example/sample && exec bash -i".to_string(),
        ];
        let cmd = remote_tasks_fetch_command(Some(&argv), "control", "/srv/example/sample");

        // Connection target preserved; tty + interactive command dropped.
        assert!(cmd.contains(&"control".to_string()));
        assert!(!cmd.iter().any(|arg| arg == "-ttt"));
        assert!(!cmd.iter().any(|arg| arg.contains("exec bash")));
        // Exactly one remote command is appended, and it owns root discovery + read.
        assert!(cmd.last().unwrap().contains("rev-parse --show-toplevel"));
        assert!(cmd.last().unwrap().contains("plan_dir=\"$root/.plan\""));
        assert!(cmd
            .last()
            .unwrap()
            .contains("plan=\"$plan_dir/tasks.json\""));
        assert!(cmd.iter().any(|arg| arg == "-oBatchMode=yes"));
    }

    #[test]
    fn plan_tasks_remote_fetch_strips_delegation_and_tty_flags_from_clusters() {
        let argv = vec![
            "ssh".to_string(),
            "-At".to_string(),
            "-vT".to_string(),
            "-oRequestTTY=force".to_string(),
            "dev".to_string(),
            "exec bash -i".to_string(),
        ];

        let cmd = remote_tasks_fetch_command(Some(&argv), "dev", "/srv/repo");

        assert!(cmd.iter().any(|arg| arg == "-v"));
        assert!(!cmd.iter().any(|arg| arg == "-A"));
        assert!(!cmd.iter().any(|arg| arg == "-At" || arg == "-vT"));
        assert!(!cmd.iter().any(|arg| arg == "-oRequestTTY=force"));
        assert!(cmd.iter().any(|arg| arg == "-T"));
    }

    #[test]
    fn plan_tasks_remote_fetch_preserves_connection_options() {
        // Port / identity options live before the target and must survive.
        let argv = vec![
            "ssh".to_string(),
            "-p".to_string(),
            "2222".to_string(),
            "-i".to_string(),
            "/tmp/user/.ssh/id_ed25519".to_string(),
            "-J".to_string(),
            "bastion".to_string(),
            "-t".to_string(),
            "user@dev".to_string(),
            "exec bash -i".to_string(),
        ];
        let cmd = remote_tasks_fetch_command(Some(&argv), "dev", "/srv/repo");

        assert!(cmd.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(cmd
            .windows(2)
            .any(|w| w == ["-i", "/tmp/user/.ssh/id_ed25519"]));
        assert!(cmd.windows(2).any(|w| w == ["-J", "bastion"]));
        assert!(cmd.contains(&"user@dev".to_string()));
        assert!(!cmd.iter().any(|arg| arg == "-t"));
        assert!(cmd.last().unwrap().contains("cwd='/srv/repo'"));
        assert!(cmd.last().unwrap().contains("git -C \"$cwd\""));
        assert!(cmd.last().unwrap().contains("plan_dir=\"$root/.plan\""));
        assert!(cmd
            .last()
            .unwrap()
            .contains("plan=\"$plan_dir/tasks.json\""));
    }

    #[test]
    fn plan_tasks_remote_fetch_preserves_clustered_connection_option_values() {
        for argv in [
            vec!["ssh", "-vp2222", "user@dev", "exec bash"],
            vec!["ssh", "-vp", "2222", "user@dev", "exec bash"],
        ] {
            let argv = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            let cmd = remote_tasks_fetch_command(Some(&argv), "dev", "/srv/repo");
            assert!(cmd.iter().any(|arg| arg == "-v"));
            assert!(
                cmd.iter().any(|arg| arg == "-p2222")
                    || cmd.windows(2).any(|window| window == ["-p", "2222"])
            );
            assert!(cmd.iter().any(|arg| arg == "user@dev"));
            assert!(!cmd.iter().any(|arg| arg == "exec bash"));
        }
    }

    #[test]
    fn plan_tasks_remote_fetch_drops_control_master_creation_flag() {
        let argv = vec![
            "ssh".to_string(),
            "-M".to_string(),
            "-i".to_string(),
            "/keys/dev".to_string(),
            "dev".to_string(),
            "exec bash".to_string(),
        ];

        let cmd = remote_tasks_fetch_command(Some(&argv), "osc-host", "/srv/repo");

        assert!(!cmd.iter().any(|arg| arg == "-M"));
        assert!(cmd.windows(2).any(|w| w == ["-i", "/keys/dev"]));
        assert!(cmd.iter().any(|arg| arg == "dev"));
        assert!(!cmd.iter().any(|arg| arg == "osc-host"));
        assert!(!cmd.iter().any(|arg| arg == "exec bash"));
    }

    #[test]
    fn plan_tasks_remote_fetch_normalizes_autossh_without_monitoring() {
        let argv = vec![
            "autossh".to_string(),
            "-M".to_string(),
            "0".to_string(),
            "-J".to_string(),
            "bastion".to_string(),
            "dev".to_string(),
            "exec bash".to_string(),
        ];

        let cmd = remote_tasks_fetch_command(Some(&argv), "osc-host", "/srv/repo");

        assert_eq!(cmd.first().map(String::as_str), Some("ssh"));
        assert!(!cmd.windows(2).any(|w| w == ["-M", "0"]));
        assert!(cmd.windows(2).any(|w| w == ["-J", "bastion"]));
        assert!(cmd.iter().any(|arg| arg == "dev"));
        assert!(!cmd.iter().any(|arg| arg == "osc-host"));
        assert!(!cmd.iter().any(|arg| arg == "exec bash"));
    }

    #[test]
    fn plan_tasks_remote_fetch_forces_bounded_compact_ssh_options() {
        let argv = vec![
            "ssh".to_string(),
            "-oBatchMode=yes".to_string(),
            "-oConnectTimeout=9".to_string(),
            "host".to_string(),
            "exec bash".to_string(),
        ];
        let cmd = remote_tasks_fetch_command(Some(&argv), "host", "/r");

        assert_eq!(
            cmd.iter()
                .filter(|arg| arg.contains("BatchMode=yes"))
                .count(),
            1
        );
        assert_eq!(
            cmd.iter()
                .filter(|arg| arg.contains("ConnectTimeout="))
                .count(),
            1
        );
        assert!(!cmd.iter().any(|arg| arg == "-oConnectTimeout=9"));
        assert!(cmd.iter().any(|arg| arg == "-oConnectTimeout=5"));
    }

    #[test]
    fn remote_probe_strips_credential_transfer_and_command_injection_options() {
        let argv = vec![
            "ssh".to_string(),
            "-i".to_string(),
            "/keys/dev".to_string(),
            "-Jbastion".to_string(),
            "-o".to_string(),
            "SendEnv=GH_TOKEN GITHUB_TOKEN".to_string(),
            "-oSetEnv=GH_TOKEN=super-secret".to_string(),
            "-oProxyCommand=sh -c 'send super-secret'".to_string(),
            "-oLocalCommand=sh -c 'leak super-secret'".to_string(),
            "-oRemoteCommand=env".to_string(),
            "-oPermitLocalCommand=yes".to_string(),
            "-F".to_string(),
            "/tmp/credential-bearing-ssh-config".to_string(),
            "-L".to_string(),
            "8080:localhost:80".to_string(),
            "-R9000:localhost:90".to_string(),
            "-D".to_string(),
            "1080".to_string(),
            "-AX".to_string(),
            "user@dev".to_string(),
            "exec bash".to_string(),
        ];

        let cmd = remote_git_identity_command(Some(&argv), "osc-host", "/srv/repo");
        let joined = cmd.join(" ");
        assert!(cmd.windows(2).any(|w| w == ["-i", "/keys/dev"]));
        assert!(cmd.iter().any(|arg| arg == "-Jbastion"));
        for forbidden in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "super-secret",
            "ProxyCommand",
            "-oLocalCommand=",
            "PermitLocalCommand=yes",
            "credential-bearing-ssh-config",
            "8080:localhost:80",
            "9000:localhost:90",
            "1080",
            "-AX",
        ] {
            assert!(
                !joined.contains(forbidden),
                "replayed unsafe option: {forbidden}"
            );
        }
        assert!(cmd.iter().any(|arg| arg == "-oClearAllForwardings=yes"));
        assert!(cmd.iter().any(|arg| arg == "-oForwardAgent=no"));
        assert!(cmd.iter().any(|arg| arg == "-oPermitLocalCommand=no"));
        assert!(cmd.iter().any(|arg| arg == "-oRemoteCommand=none"));
    }

    #[test]
    fn remote_tasks_fetch_classifies_nested_checkout_payload() {
        let output = "TAAROF_REMOTE_TASKS_V1\0ok\0/srv/repo\0{\"tasks\":[]}";
        assert_eq!(
            classify_remote_tasks_fetch(Ok(output.to_string())),
            RemoteTasksOutcome::Loaded {
                checkout_root: "/srv/repo".to_string(),
                data: PlanTasksData::from_json_str(r#"{"tasks":[]}"#).unwrap(),
            }
        );
    }

    #[test]
    fn remote_tasks_fetch_classifies_absent_plan_and_non_repository_as_neutral() {
        assert_eq!(
            classify_remote_tasks_fetch(Ok(
                "TAAROF_REMOTE_TASKS_V1\0no-plan\0/srv/repo\0".to_string()
            )),
            RemoteTasksOutcome::NoTaskSource {
                checkout_root: Some("/srv/repo".to_string()),
            }
        );
        assert_eq!(
            classify_remote_tasks_fetch(Ok("TAAROF_REMOTE_TASKS_V1\0no-repository\0".to_string())),
            RemoteTasksOutcome::NoTaskSource {
                checkout_root: None,
            }
        );
        assert_eq!(
            classify_remote_tasks_fetch(Ok("TAAROF_REMOTE_TASKS_V1\0no-plan\0".to_string())),
            RemoteTasksOutcome::Error(RemoteTasksError::Protocol)
        );
    }

    #[test]
    fn remote_tasks_fetch_classifies_transport_auth_permission_and_invalid_plans() {
        assert_eq!(
            classify_remote_tasks_fetch(Err(
                "command exited with status 255: ssh: connect to host dev port 22: Connection refused"
                    .to_string()
            )),
            RemoteTasksOutcome::Error(RemoteTasksError::Transport)
        );
        for error in [
            "command exited with status 255: Permission denied (publickey).",
            "command exited with status 255: Too many authentication failures",
            "command exited with status 255: Host key verification failed.",
        ] {
            assert_eq!(
                classify_remote_tasks_fetch(Err(error.to_string())),
                RemoteTasksOutcome::Error(RemoteTasksError::Authentication)
            );
        }
        assert_eq!(
            classify_remote_tasks_fetch(Ok(
                "TAAROF_REMOTE_TASKS_V1\0permission\0/srv/repo\0".to_string()
            )),
            RemoteTasksOutcome::Error(RemoteTasksError::Permission)
        );
        assert_eq!(
            classify_remote_tasks_fetch(Ok("TAAROF_REMOTE_TASKS_V1\0root-error\0".to_string())),
            RemoteTasksOutcome::Error(RemoteTasksError::RootDiscovery)
        );
        assert_eq!(
            classify_remote_tasks_fetch(Ok("TAAROF_REMOTE_TASKS_V1\0ok\0/srv/repo\0{".to_string())),
            RemoteTasksOutcome::Error(RemoteTasksError::MalformedJson)
        );
        assert_eq!(
            classify_remote_tasks_fetch(Ok(
                "TAAROF_REMOTE_TASKS_V1\0ok\0/srv/repo\0{\"tasks\":{}}".to_string()
            )),
            RemoteTasksOutcome::Error(RemoteTasksError::Schema)
        );
    }

    #[test]
    fn load_empty_features_returns_zero_counts() {
        let repo = temp_repo();
        write_features(&repo, r#"{"features":[]}"#);

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.total, 0);
        assert_eq!(data.done, 0);
        assert_eq!(data.queued, 0);
        assert_eq!(data.in_progress, 0);
        assert_eq!(data.percent_complete(), 0.0);
    }

    #[test]
    fn percent_complete_returns_half_for_five_of_ten() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F1","title":"1","status":"done"},
                {"id":"F2","title":"2","status":"done"},
                {"id":"F3","title":"3","status":"done"},
                {"id":"F4","title":"4","status":"done"},
                {"id":"F5","title":"5","status":"done"},
                {"id":"F6","title":"6","status":"queued"},
                {"id":"F7","title":"7","status":"queued"},
                {"id":"F8","title":"8","status":"queued"},
                {"id":"F9","title":"9","status":"in-progress"},
                {"id":"F10","title":"10","status":"in-progress"}
            ]}"#,
        );

        let data = TrackingData::load(&repo).expect("tracking data");
        assert!((data.percent_complete() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn percent_complete_is_zero_when_total_is_zero() {
        let repo = temp_repo();
        write_features(&repo, r#"{"features":[]}"#);

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.percent_complete(), 0.0);
    }

    #[test]
    fn weighted_progress_uses_effort_values() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F1","title":"quick","status":"done","effort":"quick"},
                {"id":"F2","title":"medium","status":"done","effort":"medium"},
                {"id":"F3","title":"large","status":"queued","effort":"large"}
            ]}"#,
        );

        let data = TrackingData::load(&repo).expect("tracking data");
        assert!((data.percent_complete() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn integration_cli_counts_ten_features() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F1","title":"1","status":"done"},
                {"id":"F2","title":"2","status":"done"},
                {"id":"F3","title":"3","status":"done"},
                {"id":"F4","title":"4","status":"done"},
                {"id":"F5","title":"5","status":"done"},
                {"id":"F6","title":"6","status":"queued"},
                {"id":"F7","title":"7","status":"queued"},
                {"id":"F8","title":"8","status":"queued"},
                {"id":"F9","title":"9","status":"in-progress"},
                {"id":"F10","title":"10","status":"in-progress"}
            ]}"#,
        );

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.total, 10);
        assert_eq!(data.done, 5);
    }

    #[test]
    fn integration_cli_all_done_is_full_progress() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F1","title":"1","status":"done"},
                {"id":"F2","title":"2","status":"done"}
            ]}"#,
        );

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.percent_complete(), 1.0);
    }

    #[test]
    fn integration_cli_empty_has_zero_percent() {
        let repo = temp_repo();
        write_features(&repo, r#"{"features":[]}"#);

        let data = TrackingData::load(&repo).expect("tracking data");
        assert_eq!(data.total, 0);
        assert_eq!(data.percent_complete(), 0.0);
    }

    #[test]
    fn load_tasks_returns_ready_blocked_done_counts() {
        let repo = temp_repo();
        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"Ready","status":"todo","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-2","title":"Blocked","status":"todo","blocked_by":["EXAMPLE-1"],"depends_on":[]},
                {"id":"EXAMPLE-3","title":"Doing","status":"in-progress","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-4","title":"Done","status":"done","blocked_by":[],"depends_on":[]}
            ]}"#,
        );

        let data = PlanTasksData::load(&repo).expect("plan tasks data");
        assert_eq!(data.total, 4);
        assert_eq!(data.ready, 1);
        assert_eq!(data.blocked, 1);
        assert_eq!(data.in_progress, 1);
        assert_eq!(data.done, 1);
    }

    #[test]
    fn load_tasks_sorts_in_progress_then_ready_then_blocked_then_done() {
        let repo = temp_repo();
        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-4","title":"Done","status":"done","priority":"p0","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-3","title":"Blocked","status":"todo","priority":"p0","blocked_by":["EXAMPLE-1"],"depends_on":[]},
                {"id":"EXAMPLE-2","title":"Ready","status":"todo","priority":"p1","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-1","title":"Doing","status":"in-progress","priority":"p2","blocked_by":[],"depends_on":[]}
            ]}"#,
        );

        let data = PlanTasksData::load(&repo).expect("plan tasks data");
        let ordered_ids: Vec<&str> = data.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(
            ordered_ids,
            vec!["EXAMPLE-1", "EXAMPLE-2", "EXAMPLE-3", "EXAMPLE-4"]
        );
    }

    #[test]
    fn load_tasks_blocks_tasks_with_unfinished_dependencies() {
        let repo = temp_repo();
        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"Prerequisite","status":"todo","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-2","title":"Waits","status":"todo","blocked_by":[],"depends_on":["EXAMPLE-1"]},
                {"id":"EXAMPLE-3","title":"Closed dependency","status":"done","blocked_by":[],"depends_on":[]},
                {"id":"EXAMPLE-4","title":"Ready after done dep","status":"todo","blocked_by":[],"depends_on":["EXAMPLE-3"]}
            ]}"#,
        );

        let data = PlanTasksData::load(&repo).expect("plan tasks data");
        assert_eq!(data.ready, 2);
        assert_eq!(data.blocked, 1);
        assert!(data
            .tasks
            .iter()
            .find(|task| task.id == "EXAMPLE-2")
            .unwrap()
            .is_blocked());

        let ordered_ids: Vec<&str> = data.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(
            ordered_ids,
            vec!["EXAMPLE-1", "EXAMPLE-4", "EXAMPLE-2", "EXAMPLE-3"]
        );
    }

    #[test]
    fn load_tasks_missing_file_returns_none() {
        let repo = temp_repo();
        assert!(PlanTasksData::load(&repo).is_none());
    }

    #[test]
    fn parses_depends_on_ids_and_assigned_model() {
        let data = PlanTasksData::from_json_str(
            r#"{"tasks":[
                {"id":"EXAMPLE-57","title":"A","status":"todo","depends_on":[],"assigned_model":"opus"},
                {"id":"EXAMPLE-58","title":"B","status":"todo","depends_on":["EXAMPLE-57"]}
            ]}"#,
        )
        .expect("parse");

        let a = data.tasks.iter().find(|t| t.id == "EXAMPLE-57").unwrap();
        assert_eq!(a.assigned_model.as_deref(), Some("opus"));
        assert!(a.depends_on_ids.is_empty());

        let b = data.tasks.iter().find(|t| t.id == "EXAMPLE-58").unwrap();
        assert_eq!(b.depends_on_ids, vec!["EXAMPLE-57".to_string()]);
        assert_eq!(b.assigned_model, None);
    }

    #[test]
    fn loop_runner_plan_tasks_ready_set_excludes_blocked_and_done() {
        // A done prerequisite, a ready task gated by it, an independent ready
        // task, and a task blocked by a not-done prerequisite.
        let tasks = vec![
            plan_task("DONE", "done", &[], 0),
            plan_task("READY-A", "todo", &["DONE"], 0),
            plan_task("READY-B", "todo", &[], 0),
            plan_task("BLOCKED", "todo", &["READY-B"], 1),
        ];

        let plan = compute_parallelization(&tasks);

        // The ready set is exactly the not-done, dependency-satisfied tasks.
        assert_eq!(plan.ready_now, vec!["READY-A", "READY-B"]);
        // Done tasks never appear in any wave.
        assert!(plan.waves.iter().flatten().all(|id| id != "DONE"));
        // The blocked task is not ready now; its blocker is named.
        assert!(!plan.ready_now.contains(&"BLOCKED".to_string()));
    }

    #[test]
    fn loop_runner_plan_tasks_parallel_waves_group_independent_ready_tasks() {
        // Two independent roots, each with a dependent; the two roots are one
        // parallel wave, the two dependents are the next parallel wave.
        let tasks = vec![
            plan_task("ROOT-1", "todo", &[], 0),
            plan_task("ROOT-2", "todo", &[], 0),
            plan_task("LEAF-1", "todo", &["ROOT-1"], 1),
            plan_task("LEAF-2", "todo", &["ROOT-2"], 1),
        ];

        let plan = compute_parallelization(&tasks);

        assert_eq!(plan.waves.len(), 2);
        // Wave 0: the independent roots can run in parallel.
        assert_eq!(plan.waves[0], vec!["ROOT-1", "ROOT-2"]);
        // Wave 1: the leaves unlock once their root is resolved.
        assert_eq!(plan.waves[1], vec!["LEAF-1", "LEAF-2"]);
        assert_eq!(plan.ready_now, vec!["ROOT-1", "ROOT-2"]);
        assert!(plan.blockers.is_empty());
    }

    #[test]
    fn loop_runner_plan_tasks_cyclic_dependencies_surface_as_blockers() {
        // A mutual cycle can never schedule; both tasks must surface as blocked
        // rather than spinning the wave loop forever.
        let tasks = vec![
            plan_task("CYCLE-A", "todo", &["CYCLE-B"], 1),
            plan_task("CYCLE-B", "todo", &["CYCLE-A"], 1),
        ];

        let plan = compute_parallelization(&tasks);

        assert!(plan.ready_now.is_empty());
        assert!(plan.waves.is_empty());
        let blocked_ids: Vec<&str> = plan.blockers.iter().map(|(id, _)| id.as_str()).collect();
        assert!(blocked_ids.contains(&"CYCLE-A"));
        assert!(blocked_ids.contains(&"CYCLE-B"));
    }

    #[test]
    fn plan_task_cache_reuses_projection_when_file_unchanged() {
        let repo = temp_repo();
        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"Ready","status":"todo","depends_on":[]}
            ]}"#,
        );
        let mut cache = PlanTaskProjectionCache::default();

        let first = cache.projection(&repo).expect("first projection");
        let second = cache.projection(&repo).expect("second projection");

        assert_eq!(cache.reads(), 1);
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(first.data.total, 1);
        assert_eq!(first.plan.ready_now, vec!["EXAMPLE-1"]);
    }

    #[test]
    fn plan_task_cache_reparses_when_file_changes() {
        let repo = temp_repo();
        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"Ready","status":"todo","depends_on":[]}
            ]}"#,
        );
        let mut cache = PlanTaskProjectionCache::default();
        let first = cache.projection(&repo).expect("first projection");

        write_tasks(
            &repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"Ready","status":"todo","depends_on":[]},
                {"id":"EXAMPLE-2","title":"Also ready","status":"todo","depends_on":[]}
            ]}"#,
        );
        let second = cache.projection(&repo).expect("second projection");

        assert_eq!(cache.reads(), 2);
        assert!(!Rc::ptr_eq(&first, &second));
        assert_eq!(second.data.total, 2);
        assert_eq!(second.plan.ready_now, vec!["EXAMPLE-1", "EXAMPLE-2"]);
    }

    #[test]
    fn plan_task_cache_reparses_on_checkout_switch() {
        let first_repo = temp_repo();
        let second_repo = temp_repo();
        write_tasks(
            &first_repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-1","title":"First","status":"todo","depends_on":[]}
            ]}"#,
        );
        write_tasks(
            &second_repo,
            r#"{"tasks":[
                {"id":"EXAMPLE-2","title":"Second","status":"todo","depends_on":[]}
            ]}"#,
        );
        let mut cache = PlanTaskProjectionCache::default();

        let first = cache.projection(&first_repo).expect("first repo");
        let second = cache.projection(&second_repo).expect("second repo");
        let first_again = cache.projection(&first_repo).expect("first repo again");

        assert_eq!(cache.reads(), 3);
        assert_eq!(first.data.tasks[0].id, "EXAMPLE-1");
        assert_eq!(second.data.tasks[0].id, "EXAMPLE-2");
        assert_eq!(first_again.data.tasks[0].id, "EXAMPLE-1");
        assert!(!Rc::ptr_eq(&first, &first_again));
    }

    #[test]
    fn plan_task_cache_preserves_parse_error_reporting() {
        let repo = temp_repo();
        write_tasks(&repo, "{not-json");
        let mut cache = PlanTaskProjectionCache::default();

        assert!(cache.projection(&repo).is_none());
        assert_eq!(cache.reads(), 1);
        assert!(cache.projection(&repo).is_none());
        assert_eq!(cache.reads(), 1);
    }

    #[test]
    fn plan_task_cache_reports_missing_file_as_none() {
        let repo = temp_repo();
        let mut cache = PlanTaskProjectionCache::default();

        assert!(cache.projection(&repo).is_none());
        assert_eq!(cache.reads(), 1);
        assert!(cache.projection(&repo).is_none());
        assert_eq!(cache.reads(), 1);
    }
}
