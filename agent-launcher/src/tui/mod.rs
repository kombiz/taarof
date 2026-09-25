//! Provider-neutral picker state. This module never parses commands or transcripts.
use agent_session_core::*;

#[derive(Clone, Debug)]
pub struct NewProvider {
    pub id: String,
    pub plan: ActionPlan,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKey {
    New(String),
    Session(StableRef),
}
#[derive(Clone, Debug)]
pub struct Row {
    pub key: RowKey,
    pub section: &'static str,
    pub current_repository: bool,
    pub label: String,
    pub provider: String,
    pub host: String,
    pub search: String,
    pub metadata: Vec<String>,
    pub actions: Vec<ActionPlan>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Local,
    AllHosts,
    ActiveOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    All,
    New,
    Resume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Repository,
    LastMessage,
    Grouped,
}

pub struct Picker {
    catalog: SessionCatalog,
    new_providers: Vec<NewProvider>,
    local_host: String,
    current_repository: Option<std::path::PathBuf>,
    pub selected: Option<RowKey>,
    pub query: String,
    pub provider_filter: Option<String>,
    pub scope: Scope,
    pub mode: Mode,
    pub sort: Sort,
    pub message: String,
    pub help: bool,
}
impl Picker {
    pub fn new(
        catalog: SessionCatalog,
        new_providers: Vec<NewProvider>,
        local_host: String,
    ) -> Self {
        let mut picker = Self {
            catalog,
            new_providers,
            local_host,
            current_repository: None,
            selected: None,
            query: String::new(),
            provider_filter: None,
            scope: Scope::Local,
            mode: Mode::All,
            sort: Sort::Repository,
            message: String::new(),
            help: false,
        };
        picker.selected = picker.rows().first().map(|r| r.key.clone());
        picker
    }
    pub fn new_in_directory(
        catalog: SessionCatalog,
        new_providers: Vec<NewProvider>,
        local_host: String,
        cwd: &std::path::Path,
    ) -> Self {
        let mut picker = Self::new(catalog, new_providers, local_host);
        picker.current_repository = repository_common_dir(cwd);
        picker.choose_first();
        picker
    }

    fn is_current_repository(&self, session: &SessionRecord) -> bool {
        session.source == SessionSource::LocalHistory
            && session.stable_ref.host_identity == self.local_host
            && self.current_repository.is_some()
            && session.repository_common_dir == self.current_repository
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut providers = self.new_providers.clone();
        providers.sort_by(|a, b| a.id.cmp(&b.id));
        if self.mode != Mode::Resume {
            for provider in providers {
                if provider.plan.kind != ActionKind::New {
                    continue;
                }
                let id = display_text(&provider.id);
                rows.push(Row {
                    key: RowKey::New(provider.id),
                    section: "New",
                    current_repository: false,
                    label: format!("New {id}"),
                    provider: id.clone(),
                    host: self.local_host.clone(),
                    search: id,
                    metadata: vec![
                        "Start a new provider session in the current directory.".into(),
                        format!(
                            "Directory: {}",
                            display_text(&provider.plan.cwd.to_string_lossy())
                        ),
                    ],
                    actions: vec![provider.plan],
                });
            }
        }
        if self.mode != Mode::New {
            let mut sessions: Vec<_> = self.catalog.sessions.iter().collect();
            sessions.sort_by(|a, b| {
                b.last_user_message_at_unix_ms
                    .cmp(&a.last_user_message_at_unix_ms)
                    .then_with(|| b.updated_at_unix_ms.cmp(&a.updated_at_unix_ms))
                    .then_with(|| {
                        crate::stable_ref_selector(&a.stable_ref)
                            .cmp(&crate::stable_ref_selector(&b.stable_ref))
                    })
            });
            if self.sort == Sort::Repository {
                sessions.sort_by_key(|s| !self.is_current_repository(s));
            }
            if self.sort == Sort::Grouped {
                sessions.sort_by_key(|s| !is_active(s));
            }
            for session in sessions {
                let d = &session.display;
                let provider = display_text(&d.provider);
                let host = display_text(&d.host);
                let title = display_text(&d.title);
                let mut metadata = vec![
                    format!(
                        "Last sent: {}",
                        message_time(session.last_user_message_at_unix_ms)
                    ),
                    format!("Provider: {provider}"),
                    format!("Session: {}", display_text(&d.session_id)),
                    format!("Title: {title}"),
                    format!("Host: {host}"),
                    format!("Directory: {}", display_text(&d.cwd)),
                    format!(
                        "Repository: {}",
                        d.repo_root.as_deref().map(display_text).unwrap_or_default()
                    ),
                    format!("Confidence: {:?}", session.confidence),
                    format!("Source: {:?}", session.source),
                ];
                for kind in [ActionKind::Attach, ActionKind::Resume] {
                    if !session.actions.iter().any(|action| action.kind == kind) {
                        let reason = match kind {
                            ActionKind::Attach => {
                                "no exact current live process target was validated"
                            }
                            ActionKind::Resume => {
                                "no exact provider resume authority was validated"
                            }
                            _ => unreachable!(),
                        };
                        metadata.push(format!("{} unavailable: {reason}.", kind.label()));
                    }
                }
                metadata.extend(
                    session
                        .warnings
                        .iter()
                        .map(|w| format!("Warning: {}", display_text(w))),
                );
                let mut search = metadata.join(" ");
                if let Some(binding) = &session.live_binding {
                    search.push_str(&format!(
                        " {} {}",
                        display_text(&binding.workspace_name),
                        display_text(&binding.tab_name)
                    ));
                }
                rows.push(Row {
                    key: RowKey::Session(session.stable_ref.clone()),
                    current_repository: self.is_current_repository(session),
                    section: if is_active(session) {
                        "Active"
                    } else {
                        "Recent"
                    },
                    label: format!(
                        "{} · {}{provider} · {title} · {host}",
                        format_message_time(session.last_user_message_at_unix_ms, "%m-%d %H:%M"),
                        if is_active(session) { "Active · " } else { "" }
                    ),
                    provider,
                    host: session.stable_ref.host_identity.clone(),
                    search,
                    metadata,
                    actions: session.actions.clone(),
                });
            }
        }
        rows.retain(|row| {
            self.provider_filter
                .as_ref()
                .is_none_or(|p| p == &row.provider)
                && match self.scope {
                    Scope::Local => row.host == self.local_host,
                    Scope::AllHosts => true,
                    Scope::ActiveOnly => row.section == "Active",
                }
                && fuzzy_matches(&self.query, &row.search)
        });
        rows
    }
}
fn is_active(session: &SessionRecord) -> bool {
    session.confidence == Confidence::ExactLiveBinding && session.live_binding.is_some()
}
fn fuzzy_matches(query: &str, text: &str) -> bool {
    let text = text.to_lowercase();
    query.to_lowercase().split_whitespace().all(|word| {
        let mut chars = text.chars();
        word.chars()
            .all(|needle| chars.by_ref().any(|ch| ch == needle))
    })
}
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    None,
    Refresh,
    Launch { key: RowKey, kind: ActionKind },
    Cancel,
}
impl Picker {
    fn choose_first(&mut self) {
        self.selected = self.rows().first().map(|r| r.key.clone());
    }
    pub fn selected_row(&self) -> Option<Row> {
        self.rows()
            .into_iter()
            .find(|r| Some(&r.key) == self.selected.as_ref())
    }
    pub fn key(&mut self, key: KeyEvent) -> Intent {
        if key.kind == KeyEventKind::Release {
            return Intent::None;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Intent::Cancel;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => return Intent::Refresh,
                KeyCode::Char('s') => {
                    self.sort = match self.sort {
                        Sort::Repository => Sort::LastMessage,
                        Sort::LastMessage => Sort::Grouped,
                        Sort::Grouped => Sort::Repository,
                    };
                }
                KeyCode::Char('f') => return self.launch(Some(ActionKind::Fork)),
                KeyCode::Char('n') => {
                    self.mode = Mode::All;
                    self.scope = Scope::Local;
                    self.provider_filter = None;
                    self.query.clear();
                    self.selected = self
                        .rows()
                        .iter()
                        .find(|r| r.section == "New")
                        .map(|r| r.key.clone());
                }
                KeyCode::Char('p') => {
                    let mut providers: Vec<_> = self
                        .catalog
                        .sessions
                        .iter()
                        .map(|s| display_text(&s.display.provider))
                        .chain(self.new_providers.iter().map(|p| display_text(&p.id)))
                        .collect();
                    providers.sort();
                    providers.dedup();
                    self.provider_filter = match self
                        .provider_filter
                        .as_ref()
                        .and_then(|current| providers.iter().position(|p| p == current))
                    {
                        Some(i) => providers.get(i + 1).cloned(),
                        None => providers.first().cloned(),
                    };
                    self.choose_first();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Enter => return self.launch(None),
                KeyCode::Up => self.navigate(-1),
                KeyCode::Down => self.navigate(1),
                KeyCode::Char('?') => self.help = !self.help,
                KeyCode::Tab => {
                    self.scope = match self.scope {
                        Scope::Local => Scope::AllHosts,
                        Scope::AllHosts => Scope::ActiveOnly,
                        Scope::ActiveOnly => Scope::Local,
                    };
                    self.choose_first();
                }
                KeyCode::Char(ch) if !ch.is_control() => {
                    if self.query.chars().count() < DISPLAY_FIELD_LIMIT {
                        self.query.push(ch);
                    }
                    self.choose_first();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.choose_first();
                }
                _ => {}
            }
        }
        Intent::None
    }
}
impl Picker {
    pub fn refresh(&mut self, catalog: SessionCatalog, new_providers: Vec<NewProvider>) {
        self.catalog = catalog;
        self.new_providers = new_providers;
        if self.selected.is_some() && self.selected_row().is_none() {
            self.selected = None;
            self.message =
                "Selected session disappeared or is no longer visible; choose a row to continue."
                    .into();
        } else if self.selected.is_some() {
            self.message = "Catalog refreshed.".into();
        }
    }
}

impl Picker {
    fn navigate(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let index = rows
            .iter()
            .position(|r| Some(&r.key) == self.selected.as_ref());
        let next = match index {
            Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(rows[next].key.clone());
        self.message.clear();
    }
    fn launch(&self, kind: Option<ActionKind>) -> Intent {
        let Some(row) = self.selected_row() else {
            return Intent::None;
        };
        let action = kind
            .and_then(|kind| row.actions.iter().find(|a| a.kind == kind))
            .or_else(|| {
                if kind.is_some() {
                    return None;
                }
                let defaults = if row.section == "New" {
                    vec![ActionKind::New]
                } else if row.section == "Active" {
                    vec![ActionKind::Attach, ActionKind::Resume]
                } else {
                    vec![ActionKind::Resume]
                };
                defaults
                    .iter()
                    .find_map(|kind| row.actions.iter().find(|a| a.kind == *kind))
            });
        action.map_or(Intent::None, |action| Intent::Launch {
            key: row.key.clone(),
            kind: action.kind,
        })
    }
}

mod render;
pub use render::action_meaning;
impl Picker {
    /// A rendered snapshot is never execution authority. Require the same exact
    /// identity, capability and structured plan from a fresh discovery.
    pub fn revalidate(
        &self,
        key: &RowKey,
        kind: ActionKind,
        catalog: SessionCatalog,
        new_providers: Vec<NewProvider>,
    ) -> Result<ActionPlan, String> {
        let old = self
            .rows()
            .into_iter()
            .find(|r| &r.key == key)
            .and_then(|r| r.actions.into_iter().find(|a| a.kind == kind))
            .ok_or("selected action is unavailable")?;
        if let RowKey::Session(reference) = key {
            if reference.session_id.is_empty()
                || reference.session_id.starts_with('-')
                || reference.session_id.contains('\0')
            {
                return Err("malformed session identity".into());
            }
            if catalog.sessions.iter().any(|session| {
                &session.stable_ref == reference && session.source == SessionSource::LocalHistory
            }) && catalog
                .providers
                .iter()
                .any(|p| p.name == reference.provider_id && !p.ok)
            {
                return Err("provider discovery failed; refusing launch".into());
            }
            if catalog.remote_hosts.iter().any(|h| {
                StableRef::new("", &h.host, "").host_identity == reference.host_identity
                    && (!h.ok || h.stale)
            }) {
                return Err("remote discovery is unavailable or stale".into());
            }
        }
        let mut fresh = Picker::new(catalog, new_providers, self.local_host.clone());
        fresh.scope = Scope::AllHosts;
        let plan = fresh
            .rows()
            .into_iter()
            .find(|r| &r.key == key)
            .and_then(|r| r.actions.into_iter().find(|a| a.kind == kind))
            .ok_or("selected session or action disappeared; refresh and select again")?;
        if plan != old {
            return Err("selected action changed; refresh and select again".into());
        }
        Ok(plan)
    }
}

mod runtime;
pub use runtime::{local_snapshot, run, run_for_session, run_with, Snapshot};

fn message_time(millis: Option<u64>) -> String {
    format_message_time(millis, "%Y-%m-%d %H:%M %:z")
}

fn format_message_time(millis: Option<u64>, format: &str) -> String {
    millis
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format(format)
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into())
}
