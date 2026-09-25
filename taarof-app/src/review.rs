use gtk::prelude::*;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::AppState;

const MAX_DIFF_BYTES: usize = 32 * 1024;
const MAX_FILES: usize = 250;
const CONTENT_FINGERPRINT_CHUNK_BYTES: usize = 64 * 1024;
const CONTENT_FINGERPRINT_CHUNKS: u64 = 8;
const MAX_GIT_METADATA_BYTES: usize = 1024 * 1024;
const REVIEW_COLLECTION_DEADLINE: Duration = Duration::from_secs(6);
const REFRESH_TTL_MS: u64 = 750;
const SNAPSHOT_CHANGED_MESSAGE: &str =
    "Files changed while the review snapshot was being read; refreshing";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReviewSelection {
    machine: String,
    workspace_origin: String,
    repository_hint: Option<PathBuf>,
    worktree_hint: Option<PathBuf>,
    linked_task: Option<String>,
}

impl ReviewSelection {
    fn checkout_hint(&self) -> Option<&Path> {
        self.worktree_hint
            .as_deref()
            .or(self.repository_hint.as_deref())
    }
}

pub(crate) fn selection_from_state(state: &AppState) -> Option<ReviewSelection> {
    let workspace = state.active_ws()?;
    Some(ReviewSelection {
        machine: local_machine_identity(),
        workspace_origin: workspace.work_origin.clone(),
        repository_hint: workspace.repo_root.as_deref().map(PathBuf::from),
        worktree_hint: workspace.working_tree_path.as_deref().map(PathBuf::from),
        linked_task: workspace.linked_issue.clone(),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewForgeProjection {
    pub summary: String,
    pub detail: String,
    pub url: Option<String>,
    pub identity: Option<ReviewForgeIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewForgeIdentity {
    pub checkout_root: PathBuf,
    pub branch: String,
    pub head_revision: Option<String>,
    pub remote: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewTaskProjection {
    pub summary: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RepositoryIdentity {
    machine: String,
    common_dir: PathBuf,
    worktree: PathBuf,
    branch: String,
    base_ref: Option<String>,
    base_revision: Option<String>,
    head_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ChangeLayer {
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

impl ChangeLayer {
    fn label(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FileKind {
    Text,
    Binary,
    Oversized,
    Symlink,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewFile {
    path: Vec<u8>,
    previous_path: Option<Vec<u8>>,
    status: String,
    layers: Vec<ChangeLayer>,
    kind: FileKind,
    diff: String,
    content_fingerprint: u64,
}

impl ReviewFile {
    fn display_path(&self) -> String {
        let current = String::from_utf8_lossy(&self.path).into_owned();
        self.previous_path.as_ref().map_or_else(
            || current.clone(),
            |previous| format!("{} → {current}", String::from_utf8_lossy(previous)),
        )
    }

    fn layer_label(&self) -> String {
        self.layers
            .iter()
            .map(|layer| layer.label())
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ViewedFileKey {
    repository: RepositoryIdentity,
    path: Vec<u8>,
    content_fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewSnapshot {
    selection: ReviewSelection,
    repository: Option<RepositoryIdentity>,
    files: Vec<ReviewFile>,
    captured_at_ms: u64,
    message: Option<String>,
    truncated_files: usize,
}

impl ReviewSnapshot {
    fn unavailable(selection: ReviewSelection, message: impl Into<String>) -> Self {
        Self {
            selection,
            repository: None,
            files: Vec::new(),
            captured_at_ms: now_ms(),
            message: Some(message.into()),
            truncated_files: 0,
        }
    }

    fn loading(selection: ReviewSelection) -> Self {
        Self {
            selection,
            repository: None,
            files: Vec::new(),
            captured_at_ms: 0,
            message: Some("Loading the selected worktree review…".to_string()),
            truncated_files: 0,
        }
    }

    fn renders_same_as(&self, other: &Self) -> bool {
        self.selection == other.selection
            && self.repository == other.repository
            && self.files == other.files
            && self.message == other.message
            && self.truncated_files == other.truncated_files
    }
}

#[derive(Clone)]
pub(crate) struct ReviewPanel {
    pub container: gtk::Box,
    identity: gtk::Label,
    revisions: gtk::Label,
    task_button: gtk::Button,
    forge_button: gtk::Button,
    status: gtk::Label,
    list: gtk::ListBox,
    empty: gtk::Label,
    snapshot: Rc<RefCell<Option<ReviewSnapshot>>>,
    viewed: Rc<RefCell<HashSet<ViewedFileKey>>>,
    task_url: Rc<RefCell<Option<String>>>,
    forge_url: Rc<RefCell<Option<String>>>,
    forge_projection: Rc<RefCell<ReviewForgeProjection>>,
    in_flight: Rc<Cell<bool>>,
    generation: Rc<Cell<u64>>,
    cancellation: Rc<RefCell<Option<Arc<AtomicBool>>>>,
}

pub(crate) fn build_review_panel() -> ReviewPanel {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
    container.set_hexpand(true);
    container.set_vexpand(true);

    let identity = detail_label("Select a workspace to review");
    let revisions = detail_label("Base/current revision unavailable");
    let task_button = link_button("No authoritative task link");
    let forge_button = link_button("No associated pull request");
    let status = detail_label("Review data has not been loaded yet");
    container.append(&identity);
    container.append(&revisions);
    container.append(&task_button);
    container.append(&forge_button);
    container.append(&status);

    let list = gtk::ListBox::new();
    list.add_css_class("task-panel-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(180)
        .child(&list)
        .build();
    scroll.set_vexpand(true);
    container.append(&scroll);

    let empty = gtk::Label::new(Some("No changed files for this review range."));
    empty.add_css_class("task-panel-empty");
    empty.set_halign(gtk::Align::Start);
    empty.set_wrap(true);
    container.append(&empty);

    let task_url = Rc::new(RefCell::new(None::<String>));
    let forge_url = Rc::new(RefCell::new(None::<String>));
    {
        let url = task_url.clone();
        task_button.connect_clicked(move |_| {
            if let Some(url) = url.borrow().as_deref() {
                crate::task_panel::open_external_url(url);
            }
        });
    }
    {
        let url = forge_url.clone();
        forge_button.connect_clicked(move |_| {
            if let Some(url) = url.borrow().as_deref() {
                crate::task_panel::open_external_url(url);
            }
        });
    }

    ReviewPanel {
        container,
        identity,
        revisions,
        task_button,
        forge_button,
        status,
        list,
        empty,
        snapshot: Rc::new(RefCell::new(None)),
        viewed: Rc::new(RefCell::new(HashSet::new())),
        task_url,
        forge_url,
        forge_projection: Rc::new(RefCell::new(ReviewForgeProjection::default())),
        in_flight: Rc::new(Cell::new(false)),
        generation: Rc::new(Cell::new(0)),
        cancellation: Rc::new(RefCell::new(None)),
    }
}

fn detail_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("task-panel-note");
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    label.set_selectable(true);
    label
}

fn link_button(text: &str) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    button.add_css_class("task-panel-action-button");
    button.set_halign(gtk::Align::Fill);
    button.set_sensitive(false);
    configure_link_button_label(&button);
    button
}

fn configure_link_button_label(button: &gtk::Button) {
    if let Some(label) = button.child().and_downcast::<gtk::Label>() {
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(1);
    }
}

#[derive(Clone)]
struct ReviewBudget {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl ReviewBudget {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            deadline: Instant::now() + REVIEW_COLLECTION_DEADLINE,
        }
    }

    fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("Review refresh was superseded".to_string())
        } else if Instant::now() >= self.deadline {
            Err("Review refresh exceeded the six-second read deadline".to_string())
        } else {
            Ok(())
        }
    }
}

impl ReviewPanel {
    pub(crate) fn refresh(
        &self,
        state: &Rc<RefCell<AppState>>,
        forge: ReviewForgeProjection,
        task: ReviewTaskProjection,
    ) {
        *self.forge_projection.borrow_mut() = forge.clone();
        set_link_button(
            &self.task_button,
            &self.task_url,
            &task.summary,
            task.url.as_deref(),
        );

        let selection = state
            .try_borrow()
            .ok()
            .and_then(|state| selection_from_state(&state));
        let Some(selection) = selection else {
            if let Some(cancelled) = self.cancellation.borrow_mut().take() {
                cancelled.store(true, Ordering::Relaxed);
            }
            self.generation.set(self.generation.get().wrapping_add(1));
            self.in_flight.set(false);
            self.render_forge(validated_forge_projection(&forge, None));
            self.render(ReviewSnapshot::unavailable(
                ReviewSelection {
                    machine: local_machine_identity(),
                    workspace_origin: "none".to_string(),
                    repository_hint: None,
                    worktree_hint: None,
                    linked_task: None,
                },
                "No selected workspace",
            ));
            return;
        };
        let selection_changed = self
            .snapshot
            .borrow()
            .as_ref()
            .is_none_or(|snapshot| snapshot.selection != selection);
        let current_snapshot = self.snapshot.borrow();
        let verified_repository = current_snapshot.as_ref().and_then(|snapshot| {
            (snapshot.selection == selection)
                .then_some(snapshot.repository.as_ref())
                .flatten()
        });
        self.render_forge(validated_forge_projection(&forge, verified_repository));
        drop(current_snapshot);
        if self.in_flight.get() && !selection_changed {
            return;
        }
        if self.snapshot.borrow().as_ref().is_some_and(|snapshot| {
            snapshot.selection == selection
                && now_ms().saturating_sub(snapshot.captured_at_ms) < REFRESH_TTL_MS
        }) {
            return;
        }

        if selection_changed {
            self.render(ReviewSnapshot::loading(selection.clone()));
            self.render_forge(validated_forge_projection(&forge, None));
        }

        if let Some(cancelled) = self.cancellation.borrow_mut().take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.cancellation.borrow_mut() = Some(cancelled.clone());
        self.in_flight.set(true);
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let request = selection.clone();
        let panel = self.clone();
        let state = state.clone();
        let worker_budget = ReviewBudget::new(cancelled.clone());
        let key = format!(
            "review-panel:{}:{}:{generation}",
            request.machine, request.workspace_origin
        );
        let submission = crate::git::spawn_async_result(
            key,
            move || collect_review_with_budget(request, worker_budget),
            move |result| {
                if panel.generation.get() != generation {
                    return;
                }
                panel.in_flight.set(false);
                panel.cancellation.borrow_mut().take();
                let current = state
                    .try_borrow()
                    .ok()
                    .and_then(|state| selection_from_state(&state));
                if current.as_ref() != Some(&selection) {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        let current_forge = panel.forge_projection.borrow().clone();
                        panel.render_forge(validated_forge_projection(
                            &current_forge,
                            snapshot.repository.as_ref(),
                        ));
                        panel.render(snapshot);
                    }
                    Err(error)
                        if should_preserve_rows_for_error(
                            panel.snapshot.borrow().as_ref(),
                            &selection,
                            &error,
                        ) =>
                    {
                        panel.status.set_text(&error);
                    }
                    Err(error) => {
                        let current_forge = panel.forge_projection.borrow().clone();
                        panel.render_forge(validated_forge_projection(&current_forge, None));
                        panel.render(ReviewSnapshot::unavailable(selection, error));
                    }
                }
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            cancelled.store(true, Ordering::Relaxed);
            self.cancellation.borrow_mut().take();
            self.in_flight.set(false);
            self.status
                .set_text("Review refresh is queued behind other Git work");
        }
    }

    fn render_forge(&self, forge: ReviewForgeProjection) {
        set_link_button(
            &self.forge_button,
            &self.forge_url,
            &forge.summary,
            forge.url.as_deref(),
        );
        self.forge_button.set_tooltip_text(Some(&forge.detail));
    }

    fn render(&self, snapshot: ReviewSnapshot) {
        let message = snapshot.message.clone().unwrap_or_else(|| {
            let viewed = snapshot
                .repository
                .as_ref()
                .map(|repository| {
                    snapshot
                        .files
                        .iter()
                        .filter(|file| self.viewed.borrow().contains(&viewed_key(repository, file)))
                        .count()
                })
                .unwrap_or(0);
            let suffix = if snapshot.truncated_files == 0 {
                String::new()
            } else {
                format!(" · {} more files omitted", snapshot.truncated_files)
            };
            format!(
                "{} changed files · {viewed} viewed at this exact content{suffix}",
                snapshot.files.len()
            )
        });
        self.status.set_text(&message);
        if self
            .snapshot
            .borrow()
            .as_ref()
            .is_some_and(|previous| previous.renders_same_as(&snapshot))
        {
            *self.snapshot.borrow_mut() = Some(snapshot);
            return;
        }
        let identity = snapshot.repository.as_ref().map_or_else(
            || {
                snapshot.selection.checkout_hint().map_or_else(
                    || format!("{} · no repository", snapshot.selection.machine),
                    |path| format!("{} · {}", snapshot.selection.machine, path.display()),
                )
            },
            |repo| {
                format!(
                    "{} · {} · {}",
                    repo.machine,
                    repo.common_dir.display(),
                    repo.worktree.display()
                )
            },
        );
        self.identity.set_text(&identity);
        if let Some(repo) = snapshot.repository.as_ref() {
            let base = repo.base_revision.as_deref().map_or_else(
                || "base unavailable".to_string(),
                |revision| {
                    format!(
                        "{} @ {}",
                        repo.base_ref.as_deref().unwrap_or("base"),
                        short_revision(revision)
                    )
                },
            );
            self.revisions.set_text(&format!(
                "{} · {base} → HEAD @ {}",
                repo.branch,
                short_revision(&repo.head_revision)
            ));
        } else {
            self.revisions.set_text("Base/current revision unavailable");
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for file in &snapshot.files {
            self.list.append(&self.build_file_row(&snapshot, file));
        }
        self.empty.set_visible(snapshot.files.is_empty());
        *self.snapshot.borrow_mut() = Some(snapshot);
    }

    fn build_file_row(&self, snapshot: &ReviewSnapshot, file: &ReviewFile) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.add_css_class("task-panel-row");
        let heading = gtk::Label::new(Some(&file.display_path()));
        heading.add_css_class("task-panel-task-title");
        heading.set_halign(gtk::Align::Start);
        heading.set_wrap(true);
        let detail = gtk::Label::new(Some(&format!(
            "{} · {}{}",
            file.status,
            file.layer_label(),
            match file.kind {
                FileKind::Text => "",
                FileKind::Binary => " · binary",
                FileKind::Oversized => " · oversized",
                FileKind::Symlink => " · symlink",
                FileKind::Deleted => " · deleted",
            }
        )));
        detail.add_css_class("task-panel-note");
        detail.set_halign(gtk::Align::Start);
        detail.set_wrap(true);
        let diff = gtk::Label::new(Some(&file.diff));
        diff.add_css_class("task-panel-note");
        diff.set_halign(gtk::Align::Start);
        diff.set_wrap(true);
        diff.set_selectable(true);
        diff.set_visible(false);

        let key = snapshot
            .repository
            .as_ref()
            .map(|repository| viewed_key(repository, file));
        let viewed = key
            .as_ref()
            .is_some_and(|key| self.viewed.borrow().contains(key));
        let button = gtk::Button::with_label(if viewed {
            "Viewed · show diff"
        } else {
            "View diff"
        });
        button.add_css_class("task-panel-action-button");
        button.set_halign(gtk::Align::Start);
        let viewed_set = self.viewed.clone();
        let status = self.status.clone();
        let button_for_click = button.clone();
        let diff_for_click = diff.clone();
        button.connect_clicked(move |_| {
            let show = !diff_for_click.is_visible();
            diff_for_click.set_visible(show);
            if show {
                if let Some(key) = key.as_ref() {
                    viewed_set.borrow_mut().insert(key.clone());
                }
                button_for_click.set_label("Viewed · hide diff");
                status.set_text("Viewed marker recorded for this exact revision and file content");
            } else {
                button_for_click.set_label("Viewed · show diff");
            }
        });
        content.append(&heading);
        content.append(&detail);
        content.append(&button);
        content.append(&diff);
        row.set_child(Some(&content));
        row
    }
}

fn should_preserve_rows_for_error(
    previous: Option<&ReviewSnapshot>,
    selection: &ReviewSelection,
    error: &str,
) -> bool {
    error == SNAPSHOT_CHANGED_MESSAGE
        && previous.is_some_and(|snapshot| {
            snapshot.selection == *selection && snapshot.repository.is_some()
        })
}

fn set_link_button(
    button: &gtk::Button,
    stored_url: &Rc<RefCell<Option<String>>>,
    summary: &str,
    url: Option<&str>,
) {
    button.set_label(summary);
    configure_link_button_label(button);
    button.set_sensitive(url.is_some());
    *stored_url.borrow_mut() = url.map(str::to_string);
}

fn validated_forge_projection(
    forge: &ReviewForgeProjection,
    repository: Option<&RepositoryIdentity>,
) -> ReviewForgeProjection {
    let Some(identity) = forge.identity.as_ref() else {
        return forge.clone();
    };
    if identity.remote {
        return ReviewForgeProjection {
            summary: "PR association unavailable for the remote pane".to_string(),
            detail: "Review files belong to the selected local workspace, while the focused pane has a remote Git identity."
                .to_string(),
            url: None,
            identity: forge.identity.clone(),
        };
    }
    let Some(repository) = repository else {
        return ReviewForgeProjection {
            summary: "Checking Review and PR checkout agreement…".to_string(),
            detail: "Waiting for the selected workspace's worktree, branch, and HEAD before showing PR/check state."
                .to_string(),
            url: None,
            identity: forge.identity.clone(),
        };
    };
    let Some(target_head) = identity.head_revision.as_deref() else {
        return ReviewForgeProjection {
            summary: "PR association unavailable: pane HEAD unverified".to_string(),
            detail: format!(
                "Review {} @ {} has a verified HEAD, but the focused pane's local HEAD is unavailable.",
                repository.branch,
                short_revision(&repository.head_revision)
            ),
            url: None,
            identity: forge.identity.clone(),
        };
    };
    if identity.checkout_root != repository.worktree
        || identity.branch != repository.branch
        || target_head != repository.head_revision
    {
        return ReviewForgeProjection {
            summary: "PR association is for a different checkout".to_string(),
            detail: format!(
                "Review: {} · {} @ {}. Pane PR identity: {} · {} @ {}.",
                repository.worktree.display(),
                repository.branch,
                short_revision(&repository.head_revision),
                identity.checkout_root.display(),
                identity.branch,
                short_revision(target_head)
            ),
            url: None,
            identity: forge.identity.clone(),
        };
    }
    forge.clone()
}

fn viewed_key(repository: &RepositoryIdentity, file: &ReviewFile) -> ViewedFileKey {
    ViewedFileKey {
        repository: repository.clone(),
        path: file.path.clone(),
        content_fingerprint: file.content_fingerprint,
    }
}

#[cfg(test)]
fn collect_review(selection: ReviewSelection) -> Result<ReviewSnapshot, String> {
    collect_review_with_budget(
        selection,
        ReviewBudget::new(Arc::new(AtomicBool::new(false))),
    )
}

fn collect_review_with_budget(
    selection: ReviewSelection,
    budget: ReviewBudget,
) -> Result<ReviewSnapshot, String> {
    budget.check()?;
    let Some(checkout_hint) = selection.checkout_hint() else {
        return Ok(ReviewSnapshot::unavailable(
            selection,
            "The selected workspace has no repository or worktree path",
        ));
    };
    let checkout = checkout_hint
        .canonicalize()
        .map_err(|_| "The selected worktree is unavailable".to_string())?;
    let root = git_text(&checkout, &["rev-parse", "--show-toplevel"], &budget)?;
    let worktree = PathBuf::from(root.trim());
    if worktree != checkout && !checkout.starts_with(&worktree) {
        return Err("Git resolved a worktree outside the selected workspace".to_string());
    }
    let common_dir = PathBuf::from(
        git_text(
            &worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &budget,
        )?
        .trim(),
    );
    let branch = git_text(
        &worktree,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &budget,
    )
    .map_err(|_| "Detached HEAD: review state needs a named branch".to_string())?;
    let branch = branch.trim().to_string();
    let head_revision = git_text(&worktree, &["rev-parse", "HEAD"], &budget)?
        .trim()
        .to_string();
    let (base_ref, base_revision) = resolve_base(&worktree, &branch, &head_revision, &budget);
    let before = capture_change_fingerprint(&worktree, &budget)?;
    let collected =
        collect_changed_files(&worktree, base_revision.as_deref(), &head_revision, &budget)?;
    let after = capture_change_fingerprint(&worktree, &budget)?;
    if before != after {
        return Err(SNAPSHOT_CHANGED_MESSAGE.to_string());
    }
    let repository = RepositoryIdentity {
        machine: selection.machine.clone(),
        common_dir,
        worktree,
        branch,
        base_ref,
        base_revision,
        head_revision,
    };
    Ok(ReviewSnapshot {
        selection,
        repository: Some(repository),
        files: collected.files,
        captured_at_ms: now_ms(),
        message: None,
        truncated_files: collected.truncated_files,
    })
}

fn resolve_base(
    worktree: &Path,
    branch: &str,
    head: &str,
    budget: &ReviewBudget,
) -> (Option<String>, Option<String>) {
    let origin_head = git_text(
        worktree,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        budget,
    )
    .ok()
    .map(|value| value.trim().to_string());
    let upstream = git_text(
        worktree,
        &["rev-parse", "--abbrev-ref", "@{upstream}"],
        budget,
    )
    .ok()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.ends_with(&format!("/{branch}")));
    let candidates = [origin_head, upstream];
    for candidate in candidates
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
    {
        if let Ok(base) = git_text(worktree, &["merge-base", head, &candidate], budget) {
            return (Some(candidate), Some(base.trim().to_string()));
        }
    }
    (None, None)
}

#[derive(Default)]
struct MutableFile {
    path: Vec<u8>,
    previous_path: Option<Vec<u8>>,
    statuses: Vec<String>,
    layers: HashSet<ChangeLayer>,
}

struct CollectedFiles {
    files: Vec<ReviewFile>,
    truncated_files: usize,
}

fn collect_changed_files(
    worktree: &Path,
    base_revision: Option<&str>,
    head_revision: &str,
    budget: &ReviewBudget,
) -> Result<CollectedFiles, String> {
    let mut files: BTreeMap<Vec<u8>, MutableFile> = BTreeMap::new();
    if let Some(base) = base_revision {
        let range = format!("{base}..{head_revision}");
        add_name_status(
            &mut files,
            git_bytes(
                worktree,
                &["diff", "--name-status", "-z", "--find-renames", &range],
                budget,
            )?,
            ChangeLayer::Committed,
        )?;
    }
    add_name_status(
        &mut files,
        git_bytes(
            worktree,
            &["diff", "--cached", "--name-status", "-z", "--find-renames"],
            budget,
        )?,
        ChangeLayer::Staged,
    )?;
    add_name_status(
        &mut files,
        git_bytes(
            worktree,
            &["diff", "--name-status", "-z", "--find-renames"],
            budget,
        )?,
        ChangeLayer::Unstaged,
    )?;
    for path in split_nul(git_bytes(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        budget,
    )?) {
        if safe_relative_bytes(&path) {
            let entry = files.entry(path.clone()).or_default();
            entry.path = path;
            entry.statuses.push("untracked".to_string());
            entry.layers.insert(ChangeLayer::Untracked);
        }
    }

    let truncated_files = files.len().saturating_sub(MAX_FILES);
    let mut finished = Vec::with_capacity(files.len().min(MAX_FILES));
    for file in files.into_values().take(MAX_FILES) {
        budget.check()?;
        finished.push(finish_file(
            worktree,
            base_revision,
            head_revision,
            file,
            budget,
        )?);
    }
    Ok(CollectedFiles {
        files: finished,
        truncated_files,
    })
}

fn add_name_status(
    files: &mut BTreeMap<Vec<u8>, MutableFile>,
    raw: Vec<u8>,
    layer: ChangeLayer,
) -> Result<(), String> {
    let fields = split_nul(raw);
    let mut index = 0;
    while index < fields.len() {
        let status = String::from_utf8_lossy(&fields[index]).into_owned();
        index += 1;
        let Some(first) = fields.get(index).cloned() else {
            return Err("Git returned an incomplete changed-file record".to_string());
        };
        index += 1;
        let (previous, path) = if status.starts_with('R') || status.starts_with('C') {
            let Some(second) = fields.get(index).cloned() else {
                return Err("Git returned an incomplete rename record".to_string());
            };
            index += 1;
            (Some(first), second)
        } else {
            (None, first)
        };
        if !safe_relative_bytes(&path)
            || previous
                .as_ref()
                .is_some_and(|path| !safe_relative_bytes(path))
        {
            continue;
        }
        let entry = files.entry(path.clone()).or_default();
        entry.path = path;
        entry.previous_path = previous;
        entry.statuses.push(status);
        entry.layers.insert(layer);
    }
    Ok(())
}

fn finish_file(
    worktree: &Path,
    base_revision: Option<&str>,
    head_revision: &str,
    file: MutableFile,
    budget: &ReviewBudget,
) -> Result<ReviewFile, String> {
    budget.check()?;
    let path = OsString::from_vec(file.path.clone());
    let absolute = worktree.join(&path);
    let deleted = file.statuses.iter().any(|status| status.starts_with('D')) && !absolute.exists();
    let mut diff_parts = Vec::new();
    let mut oversized = false;
    let mut binary = false;
    if file.layers.contains(&ChangeLayer::Committed) {
        let Some(base) = base_revision else {
            return Err("Committed changes have no verified base revision".to_string());
        };
        let range = format!("{base}..{head_revision}");
        let output = bounded_git_diff(
            worktree,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                &range,
            ],
            &path,
            budget,
        )?;
        oversized |= output.truncated;
        binary |= output.binary;
        if !output.text.is_empty() {
            diff_parts.push(format!("Committed\n{}", output.text));
        }
    }
    if file.layers.contains(&ChangeLayer::Staged) {
        let staged = bounded_git_diff(
            worktree,
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
            ],
            &path,
            budget,
        )?;
        oversized |= staged.truncated;
        binary |= staged.binary;
        if !staged.text.is_empty() {
            diff_parts.push(format!("Staged\n{}", staged.text));
        }
    }
    if file.layers.contains(&ChangeLayer::Unstaged) {
        let unstaged = bounded_git_diff(
            worktree,
            &["diff", "--no-color", "--no-ext-diff", "--no-textconv"],
            &path,
            budget,
        )?;
        oversized |= unstaged.truncated;
        binary |= unstaged.binary;
        if !unstaged.text.is_empty() {
            diff_parts.push(format!("Unstaged\n{}", unstaged.text));
        }
    }

    let symlink = absolute
        .symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    let mut untracked_content = None;
    if file.layers.contains(&ChangeLayer::Untracked) && !deleted {
        if symlink {
            diff_parts.push("Untracked symlink content is intentionally hidden.".to_string());
        } else if let Ok(metadata) = absolute.symlink_metadata() {
            if metadata.len() > MAX_DIFF_BYTES as u64 {
                oversized = true;
                diff_parts.push(format!(
                    "Untracked file is {} bytes; content exceeds the {} byte review limit.",
                    metadata.len(),
                    MAX_DIFF_BYTES
                ));
            } else if metadata.is_file() {
                budget.check()?;
                let mut bytes = Vec::with_capacity(metadata.len() as usize);
                std::fs::File::open(&absolute)
                    .and_then(|file| {
                        file.take((MAX_DIFF_BYTES + 1) as u64)
                            .read_to_end(&mut bytes)
                    })
                    .map_err(|_| "Could not read an untracked file".to_string())?;
                budget.check()?;
                let after = absolute
                    .symlink_metadata()
                    .map_err(|_| SNAPSHOT_CHANGED_MESSAGE.to_string())?;
                if !same_file_metadata(&metadata, &after) || bytes.len() > MAX_DIFF_BYTES {
                    return Err(SNAPSHOT_CHANGED_MESSAGE.to_string());
                }
                if bytes.contains(&0) {
                    binary = true;
                    diff_parts.push("Untracked binary file; content hidden.".to_string());
                } else {
                    diff_parts.push(format!("Untracked\n{}", String::from_utf8_lossy(&bytes)));
                }
                untracked_content = Some((metadata, bytes));
            }
        }
    }
    let kind = if deleted {
        FileKind::Deleted
    } else if symlink {
        FileKind::Symlink
    } else if oversized {
        FileKind::Oversized
    } else if binary {
        FileKind::Binary
    } else {
        FileKind::Text
    };
    let diff = if diff_parts.is_empty() {
        match kind {
            FileKind::Binary => "Binary diff; content hidden.".to_string(),
            FileKind::Deleted => "Deleted file; no text diff available.".to_string(),
            _ => "No text diff available for this change.".to_string(),
        }
    } else {
        diff_parts.join("\n\n")
    };
    let has_index_entry = !file.layers.contains(&ChangeLayer::Untracked) || file.layers.len() > 1;
    let mut layers = file.layers.into_iter().collect::<Vec<_>>();
    layers.sort();
    let status = file.statuses.join("/");
    let mut hasher = DefaultHasher::new();
    file.path.hash(&mut hasher);
    file.previous_path.hash(&mut hasher);
    status.hash(&mut hasher);
    layers.hash(&mut hasher);
    kind.hash(&mut hasher);
    diff.hash(&mut hasher);
    if has_index_entry {
        git_bytes_path(worktree, &["ls-files", "--stage", "-z"], &path, budget)?.hash(&mut hasher);
    }
    if let Some((metadata, bytes)) = untracked_content {
        hash_file_metadata(&metadata, &mut hasher);
        bytes.hash(&mut hasher);
    } else {
        hash_worktree_content(&absolute, &mut hasher, budget)?;
    }
    Ok(ReviewFile {
        path: file.path,
        previous_path: file.previous_path,
        status,
        layers,
        kind,
        diff,
        content_fingerprint: hasher.finish(),
    })
}

struct BoundedDiff {
    text: String,
    truncated: bool,
    binary: bool,
}

fn bounded_git_diff(
    worktree: &Path,
    args: &[&str],
    path: &OsStr,
    budget: &ReviewBudget,
) -> Result<BoundedDiff, String> {
    let output = run_git_bounded(worktree, args, Some(path), MAX_DIFF_BYTES, budget)?;
    if !output.success && !output.truncated {
        return Err("Git could not produce a file diff".to_string());
    }
    let bytes = output.bytes;
    let truncated = output.truncated;
    let binary = bytes.contains(&0)
        || bytes
            .windows("Binary files".len())
            .any(|window| window == b"Binary files");
    let mut text = if binary {
        "Binary diff; content hidden.".to_string()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    if truncated {
        text.push_str(&format!(
            "\n\n… diff truncated at {MAX_DIFF_BYTES} bytes (open the file externally for the remainder)"
        ));
    }
    Ok(BoundedDiff {
        text,
        truncated,
        binary,
    })
}

fn capture_change_fingerprint(worktree: &Path, budget: &ReviewBudget) -> Result<u64, String> {
    let mut hasher = DefaultHasher::new();
    git_bytes(worktree, &["rev-parse", "HEAD"], budget)?.hash(&mut hasher);
    git_bytes(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        budget,
    )?
    .hash(&mut hasher);
    git_bytes(
        worktree,
        &["diff", "--cached", "--raw", "-z", "--no-renames"],
        budget,
    )?
    .hash(&mut hasher);
    git_bytes(worktree, &["diff", "--raw", "-z", "--no-renames"], budget)?.hash(&mut hasher);
    Ok(hasher.finish())
}

fn git_text(worktree: &Path, args: &[&str], budget: &ReviewBudget) -> Result<String, String> {
    String::from_utf8(git_bytes(worktree, args, budget)?)
        .map_err(|_| "Git returned non-text repository metadata".to_string())
}

fn git_bytes(worktree: &Path, args: &[&str], budget: &ReviewBudget) -> Result<Vec<u8>, String> {
    let output = run_git_bounded(worktree, args, None, MAX_GIT_METADATA_BYTES, budget)?;
    if output.truncated {
        Err(format!(
            "Git metadata exceeded the {MAX_GIT_METADATA_BYTES} byte review limit"
        ))
    } else if output.success {
        Ok(output.bytes)
    } else {
        Err("Git could not read the selected worktree".to_string())
    }
}

fn git_bytes_path(
    worktree: &Path,
    args: &[&str],
    path: &OsStr,
    budget: &ReviewBudget,
) -> Result<Vec<u8>, String> {
    let output = run_git_bounded(worktree, args, Some(path), MAX_GIT_METADATA_BYTES, budget)?;
    if output.truncated {
        Err(format!(
            "Git metadata exceeded the {MAX_GIT_METADATA_BYTES} byte review limit"
        ))
    } else if output.success {
        Ok(output.bytes)
    } else {
        Err("Git could not read the selected worktree".to_string())
    }
}

struct BoundedGitOutput {
    bytes: Vec<u8>,
    truncated: bool,
    success: bool,
}

fn run_git_bounded(
    worktree: &Path,
    args: &[&str],
    path: Option<&OsStr>,
    byte_limit: usize,
    budget: &ReviewBudget,
) -> Result<BoundedGitOutput, String> {
    budget.check()?;
    let mut command = review_git_command(worktree);
    command.args(args);
    if let Some(path) = path {
        command.arg("--").arg(path);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    // This worker owns the child, closes its bounded pipe, and always waits.
    #[allow(clippy::disallowed_methods)]
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start Git".to_string())?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not read Git output".to_string());
    };
    let (reader_tx, reader_rx) = mpsc::sync_channel(1);
    let _reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(byte_limit.min(64 * 1024).saturating_add(1));
        let result = stdout
            .take(byte_limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = reader_tx.send(result);
    });

    let status = loop {
        match child.try_wait() {
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader_rx.recv_timeout(Duration::from_millis(100));
                return Err("Could not poll Git".to_string());
            }
            Ok(Some(status)) => break status,
            Ok(None) => {
                if let Err(error) = budget.check() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader_rx.recv_timeout(Duration::from_millis(100));
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let mut bytes = reader_rx
        .recv_timeout(Duration::from_millis(250))
        .map_err(|_| "Git output reader did not finish".to_string())?
        .map_err(|_| "Could not read Git output".to_string())?;
    let truncated = bytes.len() > byte_limit;
    bytes.truncate(byte_limit);
    Ok(BoundedGitOutput {
        bytes,
        truncated,
        success: status.success(),
    })
}

fn review_git_command(worktree: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(worktree)
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"]);
    // `--` ends option parsing, but Git still interprets magic pathspecs. Paths
    // here come from Git's changed-file list and must select their literal file.
    command.env("GIT_LITERAL_PATHSPECS", "1");
    crate::child_env::prepare_child_command(&mut command, &[]);
    command
}

fn hash_worktree_content(
    path: &Path,
    hasher: &mut DefaultHasher,
    budget: &ReviewBudget,
) -> Result<(), String> {
    budget.check()?;
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    hash_file_metadata(&metadata, hasher);
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let mut file = std::fs::File::open(path)
        .map_err(|_| "Could not fingerprint a changed file".to_string())?;
    let mut buffer = [0_u8; CONTENT_FINGERPRINT_CHUNK_BYTES];
    for offset in content_fingerprint_offsets(metadata.len()) {
        budget.check()?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| "Could not fingerprint a changed file".to_string())?;
        let read = file
            .read(&mut buffer)
            .map_err(|_| "Could not fingerprint a changed file".to_string())?;
        offset.hash(hasher);
        hasher.write(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|_| "Could not fingerprint a changed file".to_string())?;
    if !same_file_metadata(&metadata, &after) {
        return Err(SNAPSHOT_CHANGED_MESSAGE.to_string());
    }
    Ok(())
}

fn hash_file_metadata(metadata: &std::fs::Metadata, hasher: &mut DefaultHasher) {
    metadata.file_type().is_symlink().hash(hasher);
    metadata.len().hash(hasher);
    metadata.ino().hash(hasher);
    metadata.mtime().hash(hasher);
    metadata.mtime_nsec().hash(hasher);
    metadata.ctime().hash(hasher);
    metadata.ctime_nsec().hash(hasher);
}

fn same_file_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.file_type().is_file() == after.file_type().is_file()
        && before.file_type().is_dir() == after.file_type().is_dir()
        && before.file_type().is_symlink() == after.file_type().is_symlink()
        && before.len() == after.len()
        && before.ino() == after.ino()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn content_fingerprint_offsets(length: u64) -> Vec<u64> {
    let chunk_size = CONTENT_FINGERPRINT_CHUNK_BYTES as u64;
    let chunk_count = length.div_ceil(chunk_size).min(CONTENT_FINGERPRINT_CHUNKS);
    let max_offset = length.saturating_sub(chunk_size);
    (0..chunk_count)
        .map(|chunk| {
            if chunk_count <= 1 {
                0
            } else {
                max_offset.saturating_mul(chunk) / (chunk_count - 1)
            }
        })
        .collect()
}

fn split_nul(mut raw: Vec<u8>) -> Vec<Vec<u8>> {
    if raw.last() == Some(&0) {
        raw.pop();
    }
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect()
    }
}

fn safe_relative_bytes(path: &[u8]) -> bool {
    if path.is_empty() || path.contains(&0) {
        return false;
    }
    let path = PathBuf::from(OsString::from_vec(path.to_vec()));
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn local_machine_identity() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local-machine".to_string())
}

fn short_revision(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "taarof-review-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            run(&root, &["init", "-b", "main"]);
            run(&root, &["config", "user.name", "Review Test"]);
            run(&root, &["config", "user.email", "review@example.invalid"]);
            std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
            run(&root, &["add", "tracked.txt"]);
            run(&root, &["commit", "-m", "base"]);
            Self(root)
        }

        fn selection(&self, origin: &str) -> ReviewSelection {
            ReviewSelection {
                machine: "test-machine".to_string(),
                workspace_origin: origin.to_string(),
                repository_hint: Some(self.0.clone()),
                worktree_hint: Some(self.0.clone()),
                linked_task: Some("#29".to_string()),
            }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(root: &Path, args: &[&str]) {
        assert!(Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn snapshot_keeps_machine_repository_worktree_and_workspace_identity() {
        let first = TempRepo::new("switch-a");
        let second = TempRepo::new("switch-b");
        let a = collect_review(first.selection("workspace-a")).unwrap();
        let b = collect_review(second.selection("workspace-b")).unwrap();
        assert_ne!(a.selection.workspace_origin, b.selection.workspace_origin);
        assert_ne!(
            a.repository.unwrap().worktree,
            b.repository.unwrap().worktree
        );
    }

    #[test]
    fn staged_unstaged_untracked_deleted_and_renamed_files_are_distinct() {
        let repo = TempRepo::new("layers");
        std::fs::write(repo.0.join("rename-me.txt"), "rename\n").unwrap();
        std::fs::write(repo.0.join("delete-me.txt"), "delete\n").unwrap();
        run(&repo.0, &["add", "rename-me.txt", "delete-me.txt"]);
        run(&repo.0, &["commit", "-m", "more base files"]);
        std::fs::write(repo.0.join("tracked.txt"), "two\n").unwrap();
        run(&repo.0, &["add", "tracked.txt"]);
        std::fs::write(repo.0.join("tracked.txt"), "three\n").unwrap();
        std::fs::write(repo.0.join("new.txt"), "new\n").unwrap();
        run(&repo.0, &["mv", "rename-me.txt", "renamed.txt"]);
        std::fs::remove_file(repo.0.join("delete-me.txt")).unwrap();
        let snapshot = collect_review(repo.selection("layers")).unwrap();
        let tracked = snapshot
            .files
            .iter()
            .find(|file| file.path == b"tracked.txt")
            .unwrap();
        assert!(tracked.layers.contains(&ChangeLayer::Staged));
        assert!(tracked.layers.contains(&ChangeLayer::Unstaged));
        assert!(snapshot.files.iter().any(|file| {
            file.path == b"new.txt" && file.layers.contains(&ChangeLayer::Untracked)
        }));
        let renamed = snapshot
            .files
            .iter()
            .find(|file| file.path == b"renamed.txt")
            .unwrap();
        assert_eq!(
            renamed.previous_path.as_deref(),
            Some(b"rename-me.txt".as_slice())
        );
        assert!(snapshot
            .files
            .iter()
            .any(|file| file.path == b"delete-me.txt" && file.kind == FileKind::Deleted));
    }

    #[test]
    fn viewed_key_invalidates_only_the_file_edited_after_viewing() {
        let repo = TempRepo::new("viewed");
        std::fs::write(repo.0.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(repo.0.join("other.txt"), "stable\n").unwrap();
        let before = collect_review(repo.selection("viewed")).unwrap();
        let repository = before.repository.as_ref().unwrap();
        let tracked_before = before
            .files
            .iter()
            .find(|file| file.path == b"tracked.txt")
            .unwrap();
        let marker = viewed_key(repository, tracked_before);
        std::fs::write(repo.0.join("tracked.txt"), "three\n").unwrap();
        let after = collect_review(repo.selection("viewed")).unwrap();
        let tracked_after = after
            .files
            .iter()
            .find(|file| file.path == b"tracked.txt")
            .unwrap();
        assert_ne!(
            marker,
            viewed_key(after.repository.as_ref().unwrap(), tracked_after)
        );
    }

    #[test]
    fn same_size_untracked_edit_recollects_rendered_bytes_and_invalidates_viewed_key() {
        let repo = TempRepo::new("untracked-recollect");
        let path = repo.0.join("untracked.txt");
        std::fs::write(&path, "one\n").unwrap();
        let before = collect_review(repo.selection("untracked-recollect")).unwrap();
        let before_file = before
            .files
            .iter()
            .find(|file| file.path == b"untracked.txt")
            .unwrap();
        let before_key = viewed_key(before.repository.as_ref().unwrap(), before_file);

        std::fs::write(&path, "two\n").unwrap();
        let after = collect_review(repo.selection("untracked-recollect")).unwrap();
        let after_file = after
            .files
            .iter()
            .find(|file| file.path == b"untracked.txt")
            .unwrap();

        assert!(after_file.diff.contains("two"));
        assert_ne!(
            before_key,
            viewed_key(after.repository.as_ref().unwrap(), after_file)
        );
    }

    #[test]
    fn changed_filename_with_git_pathspec_magic_selects_its_literal_diff() {
        let repo = TempRepo::new("literal-pathspec");
        let magic = ":(top)tracked.txt";
        std::fs::write(repo.0.join(magic), "magic before\n").unwrap();
        run(&repo.0, &["add", "-A"]);
        run(&repo.0, &["commit", "-m", "add literal pathspec filename"]);
        std::fs::write(repo.0.join(magic), "magic after\n").unwrap();
        std::fs::write(repo.0.join("tracked.txt"), "ordinary after\n").unwrap();

        let snapshot = collect_review(repo.selection("literal-pathspec")).unwrap();
        let magic_file = snapshot
            .files
            .iter()
            .find(|file| file.path == magic.as_bytes())
            .unwrap();
        assert!(magic_file.diff.contains("magic after"));
        assert!(!magic_file.diff.contains("ordinary after"));
    }

    #[test]
    fn review_diff_never_runs_configured_textconv() {
        let repo = TempRepo::new("no-textconv");
        std::fs::write(repo.0.join(".gitattributes"), "probe.txt diff=probe\n").unwrap();
        std::fs::write(repo.0.join("probe.txt"), "before\n").unwrap();
        run(&repo.0, &["add", "-A"]);
        run(&repo.0, &["commit", "-m", "add textconv file"]);
        run(
            &repo.0,
            &[
                "config",
                "diff.probe.textconv",
                "sh -c 'touch .git/textconv-ran'",
            ],
        );
        std::fs::write(repo.0.join("probe.txt"), "after\n").unwrap();

        let snapshot = collect_review(repo.selection("no-textconv")).unwrap();
        let file = snapshot
            .files
            .iter()
            .find(|file| file.path == b"probe.txt")
            .unwrap();
        assert!(file.diff.contains("after"));
        assert!(!repo.0.join(".git/textconv-ran").exists());
    }

    #[test]
    fn branch_or_base_revision_change_invalidates_viewed_marker() {
        let repo = TempRepo::new("branch");
        std::fs::write(repo.0.join("tracked.txt"), "two\n").unwrap();
        let before = collect_review(repo.selection("branch")).unwrap();
        let file = before.files.first().unwrap();
        let marker = viewed_key(before.repository.as_ref().unwrap(), file);
        run(&repo.0, &["checkout", "-b", "other"]);
        let after = collect_review(repo.selection("branch")).unwrap();
        assert_ne!(marker.repository, after.repository.unwrap());
    }

    #[test]
    fn pushed_task_branch_uses_origin_head_instead_of_its_own_upstream() {
        let repo = TempRepo::new("pushed-base");
        let remote = std::env::temp_dir().join(format!(
            "taarof-review-pushed-base-remote-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "--bare"]);
        run(
            &repo.0,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo.0, &["push", "-u", "origin", "main"]);
        run(
            &repo.0,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        run(&repo.0, &["switch", "-c", "task/review"]);
        std::fs::write(repo.0.join("committed.txt"), "review\n").unwrap();
        run(&repo.0, &["add", "committed.txt"]);
        run(&repo.0, &["commit", "-m", "review change"]);
        run(&repo.0, &["push", "-u", "origin", "HEAD"]);

        let snapshot = collect_review(repo.selection("pushed-base")).unwrap();
        let identity = snapshot.repository.as_ref().unwrap();
        assert_eq!(identity.base_ref.as_deref(), Some("origin/main"));
        assert_ne!(
            identity.base_revision.as_deref(),
            Some(identity.head_revision.as_str())
        );
        assert!(snapshot.files.iter().any(|file| {
            file.path == b"committed.txt" && file.layers.contains(&ChangeLayer::Committed)
        }));
        std::fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn binary_oversized_and_symlink_content_are_bounded() {
        let repo = TempRepo::new("bounded");
        std::fs::write(repo.0.join("binary.bin"), [0, 1, 2, 3]).unwrap();
        std::fs::write(repo.0.join("large.txt"), vec![b'x'; MAX_DIFF_BYTES + 1]).unwrap();
        std::os::unix::fs::symlink("/outside/selected/worktree", repo.0.join("link")).unwrap();
        let snapshot = collect_review(repo.selection("bounded")).unwrap();
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|file| file.path == b"binary.bin")
                .unwrap()
                .kind,
            FileKind::Binary
        );
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|file| file.path == b"large.txt")
                .unwrap()
                .kind,
            FileKind::Oversized
        );
        assert_eq!(
            snapshot
                .files
                .iter()
                .find(|file| file.path == b"link")
                .unwrap()
                .kind,
            FileKind::Symlink
        );
    }

    #[test]
    fn equal_size_binary_oversized_and_index_edits_invalidate_viewed_markers() {
        let repo = TempRepo::new("bounded-fingerprint");
        let binary = repo.0.join("binary.bin");
        let large = repo.0.join("large.bin");
        let staged = repo.0.join("staged.bin");
        std::fs::write(&binary, [0, 1, 2, 3]).unwrap();
        let large_size =
            CONTENT_FINGERPRINT_CHUNK_BYTES * (CONTENT_FINGERPRINT_CHUNKS as usize + 2);
        std::fs::write(&large, vec![b'x'; large_size]).unwrap();
        std::fs::write(&staged, [0, 9, 8, 7]).unwrap();
        run(&repo.0, &["add", "staged.bin"]);
        std::fs::write(&staged, [0, 4, 4, 4]).unwrap();
        let before = collect_review(repo.selection("bounded-fingerprint")).unwrap();

        std::fs::write(&binary, [0, 3, 2, 1]).unwrap();
        std::fs::write(&large, vec![b'y'; large_size]).unwrap();
        std::fs::write(&staged, [0, 6, 6, 6]).unwrap();
        run(&repo.0, &["add", "staged.bin"]);
        std::fs::write(&staged, [0, 4, 4, 4]).unwrap();
        let after = collect_review(repo.selection("bounded-fingerprint")).unwrap();

        for path in [
            b"binary.bin".as_slice(),
            b"large.bin".as_slice(),
            b"staged.bin".as_slice(),
        ] {
            let before_file = before.files.iter().find(|file| file.path == path).unwrap();
            let after_file = after.files.iter().find(|file| file.path == path).unwrap();
            assert_ne!(
                viewed_key(before.repository.as_ref().unwrap(), before_file),
                viewed_key(after.repository.as_ref().unwrap(), after_file),
                "same-size content edit must invalidate {}",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn oversized_content_fingerprint_reads_at_most_eight_distributed_chunks() {
        let length = (CONTENT_FINGERPRINT_CHUNK_BYTES as u64) * 1_000;
        let offsets = content_fingerprint_offsets(length);
        assert_eq!(offsets.len(), CONTENT_FINGERPRINT_CHUNKS as usize);
        assert_eq!(offsets.first(), Some(&0));
        assert_eq!(
            offsets.last(),
            Some(&(length - CONTENT_FINGERPRINT_CHUNK_BYTES as u64))
        );
    }

    #[test]
    fn changed_file_collection_truncates_before_per_file_work() {
        let repo = TempRepo::new("many-files");
        let bulk = repo.0.join("bulk");
        std::fs::create_dir_all(&bulk).unwrap();
        for index in 0..320 {
            std::fs::write(bulk.join(format!("{index:03}.txt")), "changed\n").unwrap();
        }

        let snapshot = collect_review(repo.selection("many-files")).unwrap();

        assert_eq!(snapshot.files.len(), MAX_FILES);
        assert_eq!(snapshot.truncated_files, 70);
        assert_eq!(snapshot.files[0].path, b"bulk/000.txt");
        assert_eq!(snapshot.files[MAX_FILES - 1].path, b"bulk/249.txt");
    }

    #[test]
    fn large_tracked_binary_and_text_diffs_stay_bounded_without_hiding_other_files() {
        let repo = TempRepo::new("large-tracked");
        let binary = repo.0.join("large.bin");
        let text = repo.0.join("large.txt");
        let mut binary_file = std::fs::File::create(&binary).unwrap();
        binary_file.set_len(20 * 1024 * 1024).unwrap();
        binary_file.seek(SeekFrom::Start(10 * 1024 * 1024)).unwrap();
        std::io::Write::write_all(&mut binary_file, &[0, 1, 2, 3]).unwrap();
        std::fs::write(&text, vec![b'a'; 1024 * 1024 + 128]).unwrap();
        run(&repo.0, &["add", "large.bin", "large.txt"]);
        run(&repo.0, &["commit", "-m", "large baselines"]);

        let mut binary_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&binary)
            .unwrap();
        binary_file.seek(SeekFrom::Start(10 * 1024 * 1024)).unwrap();
        std::io::Write::write_all(&mut binary_file, &[0, 4, 5, 6]).unwrap();
        std::fs::write(&text, vec![b'b'; 1024 * 1024 + 128]).unwrap();
        std::fs::write(repo.0.join("small.txt"), "still visible\n").unwrap();

        let snapshot = collect_review(repo.selection("large-tracked")).unwrap();
        let binary = snapshot
            .files
            .iter()
            .find(|file| file.path == b"large.bin")
            .unwrap();
        let text = snapshot
            .files
            .iter()
            .find(|file| file.path == b"large.txt")
            .unwrap();
        assert_eq!(binary.kind, FileKind::Binary);
        assert_eq!(text.kind, FileKind::Oversized);
        assert!(text.diff.contains("diff truncated at 32768 bytes"));
        assert!(snapshot.files.iter().any(|file| file.path == b"small.txt"));
    }

    #[test]
    fn cancelled_collection_stops_before_git_work() {
        let repo = TempRepo::new("cancelled");
        let cancelled = Arc::new(AtomicBool::new(true));
        let error =
            collect_review_with_budget(repo.selection("cancelled"), ReviewBudget::new(cancelled))
                .unwrap_err();
        assert_eq!(error, "Review refresh was superseded");
    }

    #[test]
    fn forge_projection_requires_exact_review_checkout_branch_and_head() {
        let repo = TempRepo::new("forge-identity");
        let snapshot = collect_review(repo.selection("forge-identity")).unwrap();
        let repository = snapshot.repository.as_ref().unwrap();
        let exact = ReviewForgeProjection {
            summary: "PR #29 · open".into(),
            detail: "verified".into(),
            url: Some("https://github.com/owner/repo/pull/29".into()),
            identity: Some(ReviewForgeIdentity {
                checkout_root: repository.worktree.clone(),
                branch: repository.branch.clone(),
                head_revision: Some(repository.head_revision.clone()),
                remote: false,
            }),
        };
        assert_eq!(
            validated_forge_projection(&exact, Some(repository)).url,
            exact.url
        );

        let mut mismatched = exact.clone();
        mismatched.identity.as_mut().unwrap().checkout_root = PathBuf::from("/tmp/other-worktree");
        let projected = validated_forge_projection(&mismatched, Some(repository));
        assert_eq!(
            projected.summary,
            "PR association is for a different checkout"
        );
        assert_eq!(projected.url, None);

        mismatched.identity.as_mut().unwrap().checkout_root = repository.worktree.clone();
        mismatched.identity.as_mut().unwrap().branch = "other-branch".into();
        assert_eq!(
            validated_forge_projection(&mismatched, Some(repository)).url,
            None
        );

        mismatched.identity.as_mut().unwrap().branch = repository.branch.clone();
        mismatched.identity.as_mut().unwrap().head_revision = Some("f".repeat(40));
        assert_eq!(
            validated_forge_projection(&mismatched, Some(repository)).url,
            None
        );

        mismatched.identity.as_mut().unwrap().head_revision = None;
        assert!(validated_forge_projection(&mismatched, Some(repository))
            .summary
            .contains("HEAD unverified"));

        mismatched.identity.as_mut().unwrap().remote = true;
        assert!(validated_forge_projection(&mismatched, Some(repository))
            .summary
            .contains("remote pane"));
    }

    #[test]
    fn unsafe_git_paths_are_never_accepted_as_worktree_relative_content() {
        assert!(!safe_relative_bytes(b"../outside"));
        assert!(!safe_relative_bytes(b"/absolute"));
        assert!(safe_relative_bytes(b"dir/non-utf8-\xff"));
    }

    #[test]
    fn capture_time_alone_does_not_invalidate_rendered_file_rows() {
        let repo = TempRepo::new("capture-time");
        let selection = repo.selection("workspace-a");
        let mut earlier = collect_review(selection).expect("initial snapshot");
        let mut later = earlier.clone();
        later.captured_at_ms = earlier.captured_at_ms.saturating_add(1_000);

        assert!(earlier.renders_same_as(&later));
        earlier.message = Some("repository unavailable".to_string());
        assert!(!earlier.renders_same_as(&later));
    }

    #[test]
    fn review_git_commands_disable_optional_locks_and_sanitize_child_environment() {
        let command = review_git_command(Path::new("/tmp"));
        assert_eq!(
            command.get_args().next().and_then(OsStr::to_str),
            Some("--no-optional-locks")
        );
        let removed = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>();
        assert!(removed.contains(&OsStr::new("INFISICAL_TOKEN")));
        assert!(removed.contains(&OsStr::new("INFISICAL_SERVICE_TOKEN")));
    }

    #[test]
    fn review_git_commands_do_not_run_configured_fsmonitor() {
        let repo = TempRepo::new("no-fsmonitor");
        run(
            &repo.0,
            &[
                "config",
                "core.fsmonitor",
                "sh -c 'touch .git/fsmonitor-ran'",
            ],
        );
        std::fs::write(repo.0.join("tracked.txt"), "changed\n").unwrap();

        let snapshot = collect_review(repo.selection("no-fsmonitor")).unwrap();
        assert!(snapshot
            .files
            .iter()
            .any(|file| file.path == b"tracked.txt"));
        assert!(!repo.0.join(".git/fsmonitor-ran").exists());
    }

    #[test]
    fn torn_snapshot_preserves_only_same_selection_verified_rows() {
        let repo = TempRepo::new("torn-snapshot");
        std::fs::write(repo.0.join("tracked.txt"), "changed\n").unwrap();
        let snapshot = collect_review(repo.selection("workspace-a")).unwrap();
        assert!(should_preserve_rows_for_error(
            Some(&snapshot),
            &snapshot.selection,
            SNAPSHOT_CHANGED_MESSAGE
        ));
        assert!(!should_preserve_rows_for_error(
            Some(&snapshot),
            &repo.selection("workspace-b"),
            SNAPSHOT_CHANGED_MESSAGE
        ));
        assert!(!should_preserve_rows_for_error(
            Some(&snapshot),
            &snapshot.selection,
            "repository unavailable"
        ));
    }
}
