//! Provider-neutral machine identity, display projection, and action planning.
//! Opaque identifiers and action arguments are data, never display labels.
use crate::legacy::*;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::PathBuf;

pub const DISPLAY_FIELD_LIMIT: usize = 256;

/// Callers supply the canonical host identity, not a UI label or SSH alias.
/// DNS spelling variations normalize without resolving names or doing I/O.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct StableRef {
    pub provider_id: String,
    pub host_identity: String,
    pub session_id: String,
}
impl StableRef {
    pub fn new(provider: &str, canonical_host: &str, opaque_id: &str) -> Self {
        let canonical_host = canonical_host.trim();
        let host_identity = match canonical_host.rsplit_once('@') {
            Some((user, host)) => {
                format!("{user}@{}", host.trim_end_matches('.').to_ascii_lowercase())
            }
            None => canonical_host.trim_end_matches('.').to_ascii_lowercase(),
        };
        Self {
            provider_id: normalize_agent_name(provider),
            host_identity,
            session_id: opaque_id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    New,
    Resume,
    Attach,
    Fork,
}

impl ActionKind {
    /// User-facing continuity action. These labels keep process liveness,
    /// provider history, and saved layout restoration as separate authorities.
    pub fn label(self) -> &'static str {
        match self {
            Self::New => "Start new agent conversation",
            Self::Resume => "Resume agent conversation",
            Self::Attach => "Reattach live terminal",
            Self::Fork => "Fork agent conversation",
        }
    }

    pub fn authority_description(self) -> &'static str {
        match self {
            Self::New => "starts a new provider session in the selected directory",
            Self::Resume => {
                "relaunches the provider using an exact saved provider session identity"
            }
            Self::Attach => "reconnects to an exact current live process target",
            Self::Fork => "asks the provider to create a distinct conversation from saved history",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Local,
    Ssh,
    Taarof,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    Required,
    None,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteTarget {
    pub host_identity: String,
    /// Explicit configured SSH destination. Never inferred from a command string.
    pub ssh_target: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionPlan {
    pub kind: ActionKind,
    pub transport: Transport,
    pub program: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteTarget>,
    pub confirmation: Confirmation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<crate::AttachTarget>,
}

/// Plans the same provider invocation as the legacy copy/paste formatter.
/// No shell or compatibility command participates in planning.
pub fn plan_resume(provider: &str, cwd: PathBuf, session_id: &str) -> ActionPlan {
    let argv = match provider {
        "claude" => vec!["--resume".into(), session_id.into()],
        "codex" => vec!["resume".into(), session_id.into()],
        "pi" | "kimi" | "opencode" => vec!["--session".into(), session_id.into()],
        "copilot" => vec![format!("--resume={session_id}")],
        _ => vec![session_id.into()],
    };
    ActionPlan {
        kind: ActionKind::Resume,
        transport: Transport::Local,
        program: provider.into(),
        argv,
        cwd,
        remote: None,
        confirmation: Confirmation::Required,
        attach: None,
    }
}

/// Remove terminal escape sequences (CSI, OSC, DCS/SOS/PM/APC), Unicode bidi
/// formatting, and controls before bounding Unicode scalar values. An unfinished
/// terminal string consumes its remainder, so a truncated OSC cannot leak.
pub fn display_text(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        String,
        StringEscape,
    }
    let mut state = State::Text;
    let mut output = String::new();
    let mut count = 0;
    for ch in input.chars() {
        match state {
            State::Escape => {
                state = match ch {
                    '[' => State::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => State::String,
                    '\u{20}'..='\u{2f}' => State::Escape,
                    _ => State::Text,
                };
            }
            State::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    state = State::Text;
                }
            }
            State::String => {
                if ch == '\u{7}' || ch == '\u{9c}' {
                    state = State::Text;
                } else if ch == '\u{1b}' {
                    state = State::StringEscape;
                }
            }
            State::StringEscape => {
                state = if ch == '\\' {
                    State::Text
                } else {
                    State::String
                };
            }
            State::Text => {
                match ch {
                    '\u{1b}' => {
                        state = State::Escape;
                        continue;
                    }
                    '\u{9b}' => {
                        state = State::Csi;
                        continue;
                    }
                    '\u{90}' | '\u{98}' | '\u{9d}'..='\u{9f}' => {
                        state = State::String;
                        continue;
                    }
                    '\u{61c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}' => continue,
                    _ => {}
                }
                if ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}' {
                    continue;
                }
                if count == DISPLAY_FIELD_LIMIT {
                    break;
                }
                output.push(ch);
                count += 1;
            }
        }
    }
    output
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDisplay {
    pub provider: String,
    pub session_id: String,
    pub host: String,
    pub title: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSource {
    LocalHistory,
    RemoteHistory,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    ProviderIdentity,
    ExactLiveBinding,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Recent,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    pub stable_ref: StableRef,
    pub display: SessionDisplay,
    pub started_at_unix_ms: Option<u64>,
    pub updated_at_unix_ms: u64,
    /// Last user-authored message; absent when no reliable timestamp is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_message_at_unix_ms: Option<u64>,
    /// Local repository identity, shared by linked Git worktrees. Never resolve remote cwd locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_common_dir: Option<PathBuf>,
    pub state: SessionState,
    pub source: SessionSource,
    pub confidence: Confidence,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_binding: Option<LiveAgentBinding>,
    pub actions: Vec<ActionPlan>,
}
impl SessionRecord {
    pub fn from_legacy(record: &AgentSessionRecord, local_host: &str) -> Self {
        let stable_ref = StableRef::new(
            &record.agent,
            record.host.as_deref().unwrap_or(local_host),
            &record.session_id,
        );
        // v1 has a cwd fallback for live hints. It is not exact attach authority.
        let live_binding = record
            .live_binding
            .as_ref()
            .filter(|binding| {
                record.host.is_none()
                    && normalize_agent_name(&binding.agent) == stable_ref.provider_id
                    && binding.session_id.as_deref() == Some(record.session_id.as_str())
            })
            .cloned()
            .map(sanitize_binding);
        let actions = if record.host.is_none()
            && record.resume_unavailable_reason.is_none()
            && BUILTIN_IDS.contains(&stable_ref.provider_id.as_str())
        {
            vec![plan_resume(
                &stable_ref.provider_id,
                PathBuf::from(&record.cwd),
                &record.session_id,
            )]
        } else {
            Vec::new()
        };
        Self {
            display: SessionDisplay {
                provider: display_text(&record.agent),
                session_id: display_text(&record.session_id),
                host: display_text(record.host.as_deref().unwrap_or(local_host)),
                // Legacy titles may be excerpts of user messages. v2 has no
                // provenance for those titles, so expose metadata-only labels.
                title: display_text(&format!("{} {}", record.agent, record.session_id)),
                cwd: display_text(&record.cwd),
                repo_root: record.repo_root.as_deref().map(display_text),
            },
            stable_ref,
            started_at_unix_ms: record.started_at_unix_ms,
            updated_at_unix_ms: record.updated_at_unix_ms,
            last_user_message_at_unix_ms: record.last_user_message_at_unix_ms,
            repository_common_dir: record.host.is_none().then(|| {
                crate::repository_common_dir(std::path::Path::new(&record.cwd))
            }).flatten(),
            state: if live_binding.is_some()
                || (record.status == "active" && record.live_binding.is_none())
            {
                SessionState::Active
            } else {
                SessionState::Recent
            },
            source: if record.host.is_some() {
                SessionSource::RemoteHistory
            } else {
                SessionSource::LocalHistory
            },
            confidence: if live_binding.is_some() {
                Confidence::ExactLiveBinding
            } else {
                Confidence::ProviderIdentity
            },
            warnings: record
                .resume_unavailable_reason
                .as_deref()
                .map(display_text)
                .into_iter()
                .chain((record.live_binding.is_some() && live_binding.is_none()).then(|| "Live hint is inferred; exact process identity is unavailable and Attach is disabled.".into()))
                .collect(),
            live_binding,
            actions,
        }
    }
}
fn sanitize_binding(mut binding: LiveAgentBinding) -> LiveAgentBinding {
    binding.agent = display_text(&binding.agent);
    binding.session_id = binding.session_id.as_deref().map(display_text);
    binding.cwd = binding.cwd.as_deref().map(display_text);
    binding.workspace_name = display_text(&binding.workspace_name);
    binding.tab_name = display_text(&binding.tab_name);
    binding
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionCatalog {
    pub schema: &'static str,
    pub providers: Vec<AgentSessionProviderStatus>,
    pub sessions: Vec<SessionRecord>,
    pub remote_hosts: Vec<RemoteHostStatus>,
}
impl SessionCatalog {
    pub fn from_discovery(discovery: AgentSessionDiscovery, local_host: &str) -> Self {
        let mut sessions: Vec<_> = discovery
            .sessions
            .iter()
            .map(|record| {
                let mut normalized = SessionRecord::from_legacy(record, local_host);
                if let Some(host) = record.host.as_ref().and_then(|name| {
                    discovery
                        .remote_hosts
                        .iter()
                        .find(|host| &host.host == name)
                }) {
                    // A configured label is presentation, not machine identity.
                    // Retain the full SSH destination (including remote user).
                    if !host.ssh_target.is_empty() {
                        normalized.stable_ref =
                            StableRef::new(&record.agent, &host.ssh_target, &record.session_id);
                    }
                    if record.resume_unavailable_reason.is_none()
                        && BUILTIN_IDS.contains(&normalized.stable_ref.provider_id.as_str())
                        && !host.ssh_target.is_empty()
                    {
                        let mut plan = plan_resume(
                            &normalized.stable_ref.provider_id,
                            PathBuf::from(&record.cwd),
                            &record.session_id,
                        );
                        plan.transport = Transport::Ssh;
                        plan.remote = Some(RemoteTarget {
                            host_identity: normalized.stable_ref.host_identity.clone(),
                            ssh_target: host.ssh_target.clone(),
                        });
                        normalized.actions.push(plan);
                    }
                    normalized
                        .warnings
                        .extend(host.warning.as_deref().map(display_text));
                    normalized
                        .warnings
                        .extend(host.error.as_deref().map(display_text));
                    if host.stale {
                        normalized
                            .warnings
                            .push("Remote discovery is stale.".into());
                    }
                }
                normalized
            })
            .collect();
        // Sort before exact dedup, retaining active or newest evidence. Stable sort
        // keeps provider traversal order for ties, matching the existing catalog.
        sessions.sort_by_key(|session| {
            (
                Reverse(session.state == SessionState::Active),
                Reverse(session.updated_at_unix_ms),
            )
        });
        let mut seen = HashSet::new();
        sessions.retain(|session| seen.insert(session.stable_ref.clone()));
        let providers = discovery
            .providers
            .into_iter()
            .map(|mut status| {
                status.name = display_text(&status.name);
                status.warning = status.warning.as_deref().map(display_text);
                status.error = status.error.as_deref().map(display_text);
                status
            })
            .collect();
        let remote_hosts = discovery
            .remote_hosts
            .into_iter()
            .map(|mut host| {
                host.host = display_text(&host.host);
                host.ssh_target = display_text(&host.ssh_target);
                host.warning = host.warning.as_deref().map(display_text);
                host.error = host.error.as_deref().map(display_text);
                host
            })
            .collect();
        Self {
            schema: "agent.sessions.v2",
            providers,
            sessions,
            remote_hosts,
        }
    }
}

pub const BUILTIN_IDS: [&str; 6] = ["claude", "codex", "pi", "kimi", "opencode", "copilot"];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderMetadata {
    pub schema: &'static str,
    pub id: String,
    pub actions: Vec<ActionKind>,
}
pub trait ProviderAdapter: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;
    fn probe(&self) -> AgentSessionProviderStatus;
    fn discover(&self) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>);
    fn plan_new(&self, cwd: PathBuf) -> Option<ActionPlan>;
    fn plan_resume(&self, session: &AgentSessionRecord) -> Option<ActionPlan>;
}
struct BuiltinAdapter {
    id: &'static str,
    roots: DiscoveryRoots,
}
impl ProviderAdapter for BuiltinAdapter {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            schema: "agent.provider.v1",
            id: self.id.into(),
            actions: vec![ActionKind::New, ActionKind::Resume],
        }
    }
    fn probe(&self) -> AgentSessionProviderStatus {
        self.discover().0
    }
    fn discover(&self) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
        let roots = &self.roots;
        const LIMIT: usize = 50;
        match self.id {
            "claude" => discover_claude_sessions(roots.claude_projects_dir.as_deref(), LIMIT),
            "codex" => discover_codex_sessions(roots.codex_sessions_dir.as_deref(), LIMIT),
            "pi" => discover_pi_sessions(roots.pi_sessions_dir.as_deref(), roots.pi_session_map_path.as_deref(), LIMIT),
            "kimi" => discover_kimi_sessions(roots.kimi_session_index_path.as_deref(), roots.kimi_sessions_dir.as_deref(), LIMIT),
            "opencode" => discover_opencode_sessions(roots.opencode_db_path.as_deref(), LIMIT),
            _ => (AgentSessionProviderStatus {
                name: "copilot".into(), ok: true, history_available: false,
                warning: Some("Copilot history is unavailable on this host; only live Copilot tabs are shown.".into()),
                error: None, session_count: 0,
            }, Vec::new()),
        }
    }
    fn plan_new(&self, cwd: PathBuf) -> Option<ActionPlan> {
        Some(ActionPlan {
            kind: ActionKind::New,
            transport: Transport::Local,
            program: self.id.into(),
            argv: vec![],
            cwd,
            remote: None,
            confirmation: Confirmation::Required,
            attach: None,
        })
    }
    fn plan_resume(&self, session: &AgentSessionRecord) -> Option<ActionPlan> {
        (normalize_agent_name(&session.agent) == self.id
            && session.host.is_none()
            && session.resume_unavailable_reason.is_none())
        .then(|| plan_resume(self.id, PathBuf::from(&session.cwd), &session.session_id))
    }
}
pub struct BuiltinRegistry {
    adapters: Vec<Box<dyn ProviderAdapter>>,
}
impl BuiltinRegistry {
    pub fn new(roots: DiscoveryRoots) -> Self {
        Self {
            adapters: BUILTIN_IDS
                .into_iter()
                .map(|id| {
                    Box::new(BuiltinAdapter {
                        id,
                        roots: roots.clone(),
                    }) as Box<dyn ProviderAdapter>
                })
                .collect(),
        }
    }
    pub fn adapters(&self) -> &[Box<dyn ProviderAdapter>] {
        &self.adapters
    }
    pub fn discover(&self) -> AgentSessionDiscovery {
        let mut discovery = AgentSessionDiscovery::default();
        for adapter in &self.adapters {
            let (status, mut records) = adapter.discover();
            discovery.providers.push(status);
            discovery.sessions.append(&mut records);
        }
        discovery
    }
}
