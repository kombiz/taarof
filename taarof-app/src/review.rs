use gtk::prelude::*;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

const MAX_DIFF_BYTES: usize = 32 * 1024;
const MAX_FILES: usize = 250;
const REFRESH_TTL_MS: u64 = 750;

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
    in_flight: Rc<Cell<bool>>,
    generation: Rc<Cell<u64>>,
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
        in_flight: Rc::new(Cell::new(false)),
        generation: Rc::new(Cell::new(0)),
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
    button
}

impl ReviewPanel {
    pub(crate) fn refresh(
        &self,
        state: &Rc<RefCell<AppState>>,
        forge: ReviewForgeProjection,
        task: ReviewTaskProjection,
    ) {
        set_link_button(
            &self.forge_button,
            &self.forge_url,
            &forge.summary,
            forge.url.as_deref(),
        );
        self.forge_button.set_tooltip_text(Some(&forge.detail));
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
        if self.in_flight.get() {
            return;
        }
        if self.snapshot.borrow().as_ref().is_some_and(|snapshot| {
            snapshot.selection == selection
                && now_ms().saturating_sub(snapshot.captured_at_ms) < REFRESH_TTL_MS
        }) {
            return;
        }

        self.in_flight.set(true);
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let request = selection.clone();
        let panel = self.clone();
        let state = state.clone();
        let key = format!(
            "review-panel:{}:{}:{generation}",
            request.machine, request.workspace_origin
        );
        let submission = crate::git::spawn_async_result(
            key,
            move || collect_review(request),
            move |result| {
                panel.in_flight.set(false);
                if panel.generation.get() != generation {
                    return;
                }
                let current = state
                    .try_borrow()
                    .ok()
                    .and_then(|state| selection_from_state(&state));
                if current.as_ref() != Some(&selection) {
                    return;
                }
                match result {
                    Ok(snapshot) => panel.render(snapshot),
                    Err(error) => panel.render(ReviewSnapshot::unavailable(selection, error)),
                }
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            self.in_flight.set(false);
            self.status
                .set_text("Review refresh is queued behind other Git work");
        }
    }

    fn render(&self, snapshot: ReviewSnapshot) {
        if self.snapshot.borrow().as_ref() == Some(&snapshot) {
            return;
        }
        let identity = snapshot.repository.as_ref().map_or_else(
            || format!("{} · no repository", snapshot.selection.machine),
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

fn set_link_button(
    button: &gtk::Button,
    stored_url: &Rc<RefCell<Option<String>>>,
    summary: &str,
    url: Option<&str>,
) {
    button.set_label(summary);
    button.set_sensitive(url.is_some());
    *stored_url.borrow_mut() = url.map(str::to_string);
}

fn viewed_key(repository: &RepositoryIdentity, file: &ReviewFile) -> ViewedFileKey {
    ViewedFileKey {
        repository: repository.clone(),
        path: file.path.clone(),
        content_fingerprint: file.content_fingerprint,
    }
}

fn collect_review(selection: ReviewSelection) -> Result<ReviewSnapshot, String> {
    let Some(checkout_hint) = selection.checkout_hint() else {
        return Ok(ReviewSnapshot::unavailable(
            selection,
            "The selected workspace has no repository or worktree path",
        ));
    };
    let checkout = checkout_hint
        .canonicalize()
        .map_err(|_| "The selected worktree is unavailable".to_string())?;
    let root = git_text(&checkout, &["rev-parse", "--show-toplevel"])?;
    let worktree = PathBuf::from(root.trim());
    if worktree != checkout && !checkout.starts_with(&worktree) {
        return Err("Git resolved a worktree outside the selected workspace".to_string());
    }
    let common_dir = PathBuf::from(
        git_text(
            &worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    );
    let branch = git_text(&worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| "Detached HEAD: review state needs a named branch".to_string())?;
    let branch = branch.trim().to_string();
    let head_revision = git_text(&worktree, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let (base_ref, base_revision) = resolve_base(&worktree, &head_revision);
    let before = capture_change_fingerprint(&worktree)?;
    let mut files = collect_changed_files(&worktree, base_revision.as_deref(), &head_revision)?;
    let after = capture_change_fingerprint(&worktree)?;
    if before != after {
        return Err(
            "Files changed while the review snapshot was being read; refreshing".to_string(),
        );
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
    let truncated_files = files.len().saturating_sub(MAX_FILES);
    files.truncate(MAX_FILES);
    Ok(ReviewSnapshot {
        selection,
        repository: Some(repository),
        files,
        captured_at_ms: now_ms(),
        message: None,
        truncated_files,
    })
}

fn resolve_base(worktree: &Path, head: &str) -> (Option<String>, Option<String>) {
    let candidates = [
        git_text(worktree, &["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|value| value.trim().to_string()),
        git_text(
            worktree,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
        )
        .ok()
        .map(|value| value.trim().to_string()),
    ];
    for candidate in candidates
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
    {
        if let Ok(base) = git_text(worktree, &["merge-base", head, &candidate]) {
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

fn collect_changed_files(
    worktree: &Path,
    base_revision: Option<&str>,
    head_revision: &str,
) -> Result<Vec<ReviewFile>, String> {
    let mut files: BTreeMap<Vec<u8>, MutableFile> = BTreeMap::new();
    if let Some(base) = base_revision {
        let range = format!("{base}..{head_revision}");
        add_name_status(
            &mut files,
            git_bytes(
                worktree,
                &["diff", "--name-status", "-z", "--find-renames", &range],
            )?,
            ChangeLayer::Committed,
        )?;
    }
    add_name_status(
        &mut files,
        git_bytes(
            worktree,
            &["diff", "--cached", "--name-status", "-z", "--find-renames"],
        )?,
        ChangeLayer::Staged,
    )?;
    add_name_status(
        &mut files,
        git_bytes(worktree, &["diff", "--name-status", "-z", "--find-renames"])?,
        ChangeLayer::Unstaged,
    )?;
    for path in split_nul(git_bytes(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?) {
        if safe_relative_bytes(&path) {
            let entry = files.entry(path.clone()).or_default();
            entry.path = path;
            entry.statuses.push("untracked".to_string());
            entry.layers.insert(ChangeLayer::Untracked);
        }
    }

    files
        .into_values()
        .map(|file| finish_file(worktree, base_revision, head_revision, file))
        .collect()
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
) -> Result<ReviewFile, String> {
    let path = OsString::from_vec(file.path.clone());
    let absolute = worktree.join(&path);
    let deleted = file.statuses.iter().any(|status| status.starts_with('D')) && !absolute.exists();
    let mut diff_parts = Vec::new();
    let mut oversized = false;
    let mut binary = false;
    if let Some(base) = base_revision {
        let range = format!("{base}..{head_revision}");
        let output = bounded_git_diff(
            worktree,
            &["diff", "--no-color", "--no-ext-diff", &range],
            &path,
        )?;
        oversized |= output.truncated;
        binary |= output.binary;
        if !output.text.is_empty() {
            diff_parts.push(format!("Committed\n{}", output.text));
        }
    }
    let staged = bounded_git_diff(
        worktree,
        &["diff", "--cached", "--no-color", "--no-ext-diff"],
        &path,
    )?;
    oversized |= staged.truncated;
    binary |= staged.binary;
    if !staged.text.is_empty() {
        diff_parts.push(format!("Staged\n{}", staged.text));
    }
    let unstaged = bounded_git_diff(worktree, &["diff", "--no-color", "--no-ext-diff"], &path)?;
    oversized |= unstaged.truncated;
    binary |= unstaged.binary;
    if !unstaged.text.is_empty() {
        diff_parts.push(format!("Unstaged\n{}", unstaged.text));
    }

    let symlink = absolute
        .symlink_metadata()
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    if file.layers.contains(&ChangeLayer::Untracked) && !deleted {
        if symlink {
            diff_parts.push("Untracked symlink content is intentionally hidden.".to_string());
        } else if let Ok(metadata) = absolute.metadata() {
            if metadata.len() > MAX_DIFF_BYTES as u64 {
                oversized = true;
                diff_parts.push(format!(
                    "Untracked file is {} bytes; content exceeds the {} byte review limit.",
                    metadata.len(),
                    MAX_DIFF_BYTES
                ));
            } else if metadata.is_file() {
                let mut bytes = Vec::new();
                std::fs::File::open(&absolute)
                    .and_then(|mut file| file.read_to_end(&mut bytes))
                    .map_err(|_| "Could not read an untracked file".to_string())?;
                if bytes.contains(&0) {
                    binary = true;
                    diff_parts.push("Untracked binary file; content hidden.".to_string());
                } else {
                    diff_parts.push(format!("Untracked\n{}", String::from_utf8_lossy(&bytes)));
                }
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

fn bounded_git_diff(worktree: &Path, args: &[&str], path: &OsStr) -> Result<BoundedDiff, String> {
    let mut command = Command::new("git");
    command
        .current_dir(worktree)
        .args(args)
        .arg("--")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // This worker owns the child and always reaps it with `wait()` below; it
    // needs the live stdout handle so oversized diffs can be killed at the cap.
    #[allow(clippy::disallowed_methods)]
    let mut child = command
        .spawn()
        .map_err(|_| "Could not start Git diff".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not read Git diff".to_string())?;
    let mut bytes = Vec::with_capacity(MAX_DIFF_BYTES + 1);
    stdout
        .by_ref()
        .take((MAX_DIFF_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read Git diff".to_string())?;
    let truncated = bytes.len() > MAX_DIFF_BYTES;
    if truncated {
        let _ = child.kill();
        bytes.truncate(MAX_DIFF_BYTES);
    }
    let status = child
        .wait()
        .map_err(|_| "Could not finish Git diff".to_string())?;
    if !status.success() && !truncated {
        return Err("Git could not produce a file diff".to_string());
    }
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

fn capture_change_fingerprint(worktree: &Path) -> Result<u64, String> {
    let mut hasher = DefaultHasher::new();
    git_bytes(worktree, &["rev-parse", "HEAD"])?.hash(&mut hasher);
    git_bytes(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?
    .hash(&mut hasher);
    git_bytes(worktree, &["diff", "--cached", "--binary"])?.hash(&mut hasher);
    git_bytes(worktree, &["diff", "--binary"])?.hash(&mut hasher);
    Ok(hasher.finish())
}

fn git_text(worktree: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git_bytes(worktree, args)?)
        .map_err(|_| "Git returned non-text repository metadata".to_string())
}

fn git_bytes(worktree: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .current_dir(worktree)
        .args(args)
        .output()
        .map_err(|_| "Could not start Git".to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err("Git could not read the selected worktree".to_string())
    }
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
    fn unsafe_git_paths_are_never_accepted_as_worktree_relative_content() {
        assert!(!safe_relative_bytes(b"../outside"));
        assert!(!safe_relative_bytes(b"/absolute"));
        assert!(safe_relative_bytes(b"dir/non-utf8-\xff"));
    }
}
