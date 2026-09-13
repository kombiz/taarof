//! Explicitly trusted same-user executable providers. Discovery never grants trust.
use crate::*;
use serde::Deserialize;
use std::{collections::HashSet, path::Path};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub display_name: String,
    pub command: Vec<String>,
    pub enabled: bool,
    pub capabilities: Vec<String>,
}
#[derive(Clone)]
pub struct ExternalAdapter {
    pub manifest: Manifest,
}
#[derive(Default)]
pub struct ExternalRegistry {
    adapters: Vec<ExternalAdapter>,
    diagnostics: Vec<AgentSessionProviderStatus>,
}
fn unavailable(id: &str, error: &str) -> AgentSessionProviderStatus {
    AgentSessionProviderStatus {
        name: display_text(id),
        ok: false,
        history_available: false,
        warning: Some("Explicitly enabled external adapter: same-user executable code".into()),
        error: Some(error.into()),
        session_count: 0,
    }
}
impl ExternalRegistry {
    pub fn load(dir: &Path) -> Self {
        let mut registry = Self::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return registry,
            Err(_) => {
                registry.diagnostics.push(unavailable(
                    "external",
                    "cannot read provider manifest directory",
                ));
                return registry;
            }
        };
        let mut paths = Vec::new();
        for (index, entry) in entries.take(65).enumerate() {
            if index == 64 {
                registry.diagnostics.push(unavailable(
                    "external",
                    "too many provider directory entries (maximum 64)",
                ));
                return registry;
            }
            match entry {
                Ok(entry) => {
                    if entry.path().extension().is_some_and(|e| e == "toml") {
                        paths.push(entry.path());
                    }
                }
                Err(_) => registry
                    .diagnostics
                    .push(unavailable("external", "cannot read manifest entry")),
            }
        }
        paths.sort();
        if paths.len() > 64 {
            registry.diagnostics.push(unavailable(
                "external",
                "too many provider manifests (maximum 64)",
            ));
            return registry;
        }
        let mut seen = HashSet::new();
        let mut duplicates = HashSet::new();
        for path in paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let parsed = (|| {
                use std::{io::Read, os::unix::fs::OpenOptionsExt};
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|_| "manifest unreadable")?;
                if !file
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.len() <= 16384)
                {
                    return Err("manifest unavailable or oversized");
                }
                let mut text = String::new();
                file.take(16385)
                    .read_to_string(&mut text)
                    .map_err(|_| "manifest unreadable")?;
                if text.len() > 16384 {
                    return Err("manifest oversized");
                }
                let value: toml::Value = toml::from_str(&text).map_err(|_| "malformed manifest")?;
                if value.get("schema").and_then(toml::Value::as_integer) != Some(1) {
                    return Err("unsupported manifest schema");
                }
                let manifest: Manifest =
                    toml::from_str(&text).map_err(|_| "invalid manifest fields")?;
                if manifest.id.is_empty()
                    || manifest.id.len() > 64
                    || !manifest
                        .id
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                {
                    return Err("invalid provider id");
                }
                if BUILTIN_IDS.contains(&crate::legacy::normalize_agent_name(&manifest.id).as_str())
                {
                    return Err("built-in replacement is not configured or supported");
                }
                if manifest.command.is_empty()
                    || manifest.command[0].is_empty()
                    || manifest.command.iter().any(|s| s.contains('\0'))
                {
                    return Err("invalid adapter command");
                }
                let mut caps = HashSet::new();
                if manifest
                    .capabilities
                    .iter()
                    .any(|c| !matches!(c.as_str(), "new" | "resume") || !caps.insert(c))
                {
                    return Err("unsupported or duplicate capability");
                }
                Ok(manifest)
            })();
            match parsed {
                Ok(manifest) => {
                    if !seen.insert(manifest.id.clone()) {
                        duplicates.insert(manifest.id.clone());
                    }
                    if manifest.enabled {
                        registry.adapters.push(ExternalAdapter { manifest });
                    }
                }
                Err(error) => registry.diagnostics.push(unavailable(&name, error)),
            }
        }
        registry
            .adapters
            .retain(|a| !duplicates.contains(&a.manifest.id));
        for id in duplicates {
            registry
                .diagnostics
                .push(unavailable(&id, "duplicate provider id"));
        }
        registry
    }
    pub fn adapters(&self) -> &[ExternalAdapter] {
        &self.adapters
    }
    pub fn diagnostics(&self) -> &[AgentSessionProviderStatus] {
        &self.diagnostics
    }
}
mod process;
use crate::protocol;
use std::path::PathBuf;
impl ExternalAdapter {
    fn request<T: serde::de::DeserializeOwned>(
        &self,
        operation: &str,
        cwd: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<T, String> {
        let mut request = serde_json::to_vec(&protocol::Request {
            protocol: 1,
            operation,
            limit: 50,
            cwd,
            session_id,
        })
        .map_err(|_| "adapter request invalid")?;
        request.push(b'\n');
        let bytes = process::invoke(&self.manifest.command, &request)?;
        serde_json::from_slice(&bytes).map_err(|_| "malformed adapter protocol response".into())
    }
    fn verify_metadata(&self) -> Result<(), String> {
        let metadata: protocol::Metadata = self.request("metadata", None, None)?;
        let mut actual = metadata.capabilities.clone();
        actual.sort();
        let mut declared = self.manifest.capabilities.clone();
        declared.sort();
        if metadata.protocol != 1
            || metadata.id != self.manifest.id
            || actual != declared
            || display_text(&metadata.display_name) != display_text(&self.manifest.display_name)
        {
            return Err("adapter metadata/protocol/capabilities mismatch".into());
        }
        Ok(())
    }
    fn verify_available(&self) -> Result<(), String> {
        self.verify_metadata()?;
        let probe: protocol::Probe = self.request("probe", None, None)?;
        if probe.protocol != 1 || !probe.available {
            return Err("adapter probe unavailable or protocol mismatch".into());
        }
        Ok(())
    }
    pub fn checked_plan(
        &self,
        kind: ActionKind,
        cwd: &Path,
        session_id: Option<&str>,
    ) -> Result<ActionPlan, String> {
        let operation = match kind {
            ActionKind::New => "plan-new",
            ActionKind::Resume => "plan-resume",
            _ => return Err("unsupported adapter action".into()),
        };
        let capability = if kind == ActionKind::New {
            "new"
        } else {
            "resume"
        };
        if !self.manifest.capabilities.iter().any(|c| c == capability) {
            return Err("adapter capability unavailable".into());
        }
        self.verify_available()?;
        let cwd = cwd
            .to_str()
            .filter(|p| Path::new(p).is_absolute())
            .ok_or("invalid action cwd")?;
        let plan: protocol::Plan = self.request(operation, Some(cwd), session_id)?;
        if plan.protocol != 1
            || plan.cwd != cwd
            || plan.program.is_empty()
            || plan.program.starts_with('-')
            || plan.program.contains('\0')
            || plan.argv.len() > 128
            || plan.argv.iter().any(|a| a.contains('\0') || a.len() > 8192)
        {
            return Err("invalid adapter action plan".into());
        }
        // Resolve using the launcher's execution PATH, not the adapter's fixed
        // discovery PATH, and pin the selected executable into the reviewed plan.
        use std::os::unix::fs::PermissionsExt;
        let candidates: Vec<PathBuf> = if plan.program.contains('/') {
            vec![PathBuf::from(&plan.program)]
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|path| path.join(&plan.program))
                .collect()
        };
        let program = candidates
            .into_iter()
            .find(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .and_then(|path| std::fs::canonicalize(path).ok())
            .and_then(|path| path.into_os_string().into_string().ok())
            .ok_or("adapter launch executable unavailable")?;
        Ok(ActionPlan {
            attach: None,
            kind,
            transport: Transport::Local,
            program,
            argv: plan.argv,
            cwd: plan.cwd.into(),
            remote: None,
            confirmation: Confirmation::Required,
        })
    }
    fn checked_discover(&self) -> Result<Vec<AgentSessionRecord>, String> {
        self.verify_available()?;
        let discovery: protocol::Discovery = self.request("discover", None, None)?;
        if discovery.protocol != 1 || discovery.sessions.len() > 50 {
            return Err("adapter discovery protocol or record limit".into());
        }
        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for session in discovery.sessions {
            if session.session_id.is_empty()
                || session.session_id.len() > 8192
                || session.session_id.contains('\0')
                || !seen.insert(session.session_id.clone())
                || !Path::new(&session.cwd).is_absolute()
                || session.cwd.contains('\0')
            {
                return Err("invalid or duplicate adapter session".into());
            }
            records.push(AgentSessionRecord {
                agent: self.manifest.id.clone(),
                session_id: session.session_id,
                title: session.title,
                cwd: session.cwd,
                host: None,
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: session.updated_at_unix_ms,
                last_user_message_at_unix_ms: None,
                status: "recent".into(),
                live_binding: None,
                resume_command: None,
                resume_unavailable_reason: None,
            });
        }
        Ok(records)
    }
    pub fn check_conformance(&self, cwd: &Path) -> Result<(), String> {
        let records = self.checked_discover()?;
        if self.manifest.capabilities.iter().any(|c| c == "new") {
            self.checked_plan(ActionKind::New, cwd, None)?;
        }
        if self.manifest.capabilities.iter().any(|c| c == "resume") {
            let record = records
                .first()
                .ok_or("resume conformance requires a synthetic session fixture")?;
            self.checked_plan(
                ActionKind::Resume,
                Path::new(&record.cwd),
                Some(&record.session_id),
            )?;
        }
        Ok(())
    }
}
impl ProviderAdapter for ExternalAdapter {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            schema: "agent.provider.v1",
            id: self.manifest.id.clone(),
            actions: self
                .manifest
                .capabilities
                .iter()
                .map(|c| {
                    if c == "new" {
                        ActionKind::New
                    } else {
                        ActionKind::Resume
                    }
                })
                .collect(),
        }
    }
    fn probe(&self) -> AgentSessionProviderStatus {
        self.discover().0
    }
    fn discover(&self) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
        match self.checked_discover() {
            Ok(records) => (
                AgentSessionProviderStatus {
                    name: self.manifest.id.clone(),
                    ok: true,
                    history_available: true,
                    warning: Some(
                        "Explicitly enabled external adapter: same-user executable code".into(),
                    ),
                    error: None,
                    session_count: records.len(),
                },
                records,
            ),
            Err(error) => (unavailable(&self.manifest.id, &error), vec![]),
        }
    }
    fn plan_new(&self, cwd: PathBuf) -> Option<ActionPlan> {
        self.checked_plan(ActionKind::New, &cwd, None).ok()
    }
    fn plan_resume(&self, session: &AgentSessionRecord) -> Option<ActionPlan> {
        if session.agent != self.manifest.id || session.host.is_some() {
            return None;
        }
        self.checked_plan(
            ActionKind::Resume,
            Path::new(&session.cwd),
            Some(&session.session_id),
        )
        .ok()
    }
}
impl ExternalRegistry {
    pub fn merge_catalog(&self, catalog: &mut SessionCatalog, host: &str) {
        catalog.providers.extend(self.diagnostics.clone());
        for adapter in &self.adapters {
            let (mut status, records) = adapter.discover();
            let mut sessions = Vec::new();
            for record in records {
                let mut session = SessionRecord::from_legacy(&record, host);
                if adapter.manifest.capabilities.iter().any(|c| c == "resume") {
                    match adapter.checked_plan(
                        ActionKind::Resume,
                        Path::new(&record.cwd),
                        Some(&record.session_id),
                    ) {
                        Ok(plan) => session.actions.push(plan),
                        Err(error) => {
                            status = unavailable(&adapter.manifest.id, &error);
                            break;
                        }
                    }
                }
                sessions.push(session);
            }
            if status.ok {
                catalog.sessions.extend(sessions);
            }
            catalog.providers.push(status);
        }
        catalog.sessions.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.state == SessionState::Active),
                std::cmp::Reverse(s.updated_at_unix_ms),
            )
        });
    }
}
