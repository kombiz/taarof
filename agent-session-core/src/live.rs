//! Exact, fresh live evidence supplied by Taarof; no cwd inference is authority.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachTarget {
    pub stable_ref: StableRef,
    pub workspace_id: u32,
    pub tab_id: u32,
    pub pane_id: u32,
    pub session_name: String,
    pub ssh_target: Option<String>,
}
#[derive(Clone, Debug)]
pub struct LiveSessionEvidence {
    pub stable_ref: StableRef,
    pub binding: LiveAgentBinding,
    pub cwd: PathBuf,
    pub tmux_session: Option<String>,
    pub ssh_target: Option<String>,
}
impl LiveSessionEvidence {
    pub fn attach_target(&self) -> Option<AttachTarget> {
        Some(AttachTarget {
            stable_ref: self.stable_ref.clone(),
            workspace_id: self.binding.workspace_id,
            tab_id: self.binding.tab_id,
            pane_id: self.binding.pane_id,
            session_name: self.tmux_session.clone()?,
            ssh_target: self.ssh_target.clone(),
        })
    }
}

/// Recheck at the socket mutation boundary, including ambiguity and the full
/// tmux destination. Matching a pane number or a cwd alone is never sufficient.
pub fn validate_live_attach(
    target: &AttachTarget,
    live: &[LiveSessionEvidence],
) -> Result<(), String> {
    let matches: Vec<_> = live
        .iter()
        .filter(|e| e.stable_ref == target.stable_ref)
        .collect();
    if matches.len() != 1 {
        return Err("Live session vanished or is ambiguous; refresh the catalog.".into());
    }
    if matches[0].attach_target().as_ref() != Some(target) {
        return Err("Live tmux binding changed; refresh the catalog.".into());
    }
    Ok(())
}

pub fn enrich_live(catalog: &mut SessionCatalog, live: &[LiveSessionEvidence]) {
    let mut by_ref: HashMap<&StableRef, Vec<&LiveSessionEvidence>> = HashMap::new();
    for entry in live {
        by_ref.entry(&entry.stable_ref).or_default().push(entry);
    }
    // A process resume token can be a name or prefix. Only an existing provider
    // history ID can be upgraded; never invent a stable identity from that token.
    for row in &mut catalog.sessions {
        row.actions.retain(|a| a.kind != ActionKind::Attach);
        let matches = by_ref.get(&row.stable_ref);
        row.live_binding = None;
        row.confidence = Confidence::ProviderIdentity;
        row.state = SessionState::Recent;
        let Some(matches) = matches else {
            continue;
        };
        if matches.len() != 1 {
            row.warnings.push(
                "Multiple live panes share this exact identity; attach is unavailable.".into(),
            );
            continue;
        }
        let entry = matches[0];
        row.state = SessionState::Active;
        row.confidence = Confidence::ExactLiveBinding;
        let mut binding = entry.binding.clone();
        binding.agent = display_text(&binding.agent);
        binding.session_id = binding.session_id.as_deref().map(display_text);
        binding.cwd = binding.cwd.as_deref().map(display_text);
        binding.workspace_name = display_text(&binding.workspace_name);
        binding.tab_name = display_text(&binding.tab_name);
        row.live_binding = Some(binding);
        if let Some(target) = entry.attach_target() {
            row.actions.insert(
                0,
                ActionPlan {
                    kind: ActionKind::Attach,
                    transport: Transport::Taarof,
                    program: "taarof".into(),
                    argv: vec![],
                    cwd: entry.cwd.clone(),
                    remote: None,
                    confirmation: Confirmation::Required,
                    attach: Some(target),
                },
            );
        }
    }
    catalog.sessions.sort_by_key(|s| {
        (
            std::cmp::Reverse(s.state == SessionState::Active),
            std::cmp::Reverse(s.updated_at_unix_ms),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence(host: &str) -> LiveSessionEvidence {
        LiveSessionEvidence {
            stable_ref: StableRef::new("codex", host, "exact"),
            cwd: "/synthetic".into(),
            tmux_session: Some("same-name".into()),
            ssh_target: Some(host.into()),
            binding: LiveAgentBinding {
                agent: "codex".into(),
                session_id: Some("exact".into()),
                cwd: Some("/synthetic".into()),
                workspace_id: 1,
                workspace_name: "ws".into(),
                tab_id: 2,
                tab_name: "tab".into(),
                pane_id: 3,
            },
        }
    }
    #[test]
    fn exact_live_binding_upgrades_resume_row_with_attach() {
        let mut catalog = SessionCatalog::from_discovery(AgentSessionDiscovery::default(), "local");
        let e = evidence("one.ts");
        let mut row = SessionRecord::from_legacy(
            &AgentSessionRecord {
                agent: "codex".into(),
                session_id: "exact".into(),
                title: String::new(),
                cwd: "/synthetic".into(),
                host: None,
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: 1,
                last_user_message_at_unix_ms: None,
                status: "recent".into(),
                live_binding: None,
                resume_command: None,
                resume_unavailable_reason: None,
            },
            "one.ts",
        );
        assert_eq!(row.actions[0].kind, ActionKind::Resume);
        row.stable_ref = e.stable_ref.clone();
        catalog.sessions.push(row);
        enrich_live(&mut catalog, std::slice::from_ref(&e));
        assert_eq!(catalog.sessions[0].actions[0].kind, ActionKind::Attach);
        assert_eq!(catalog.sessions[0].actions[1].kind, ActionKind::Resume);
        assert!(validate_live_attach(&e.attach_target().unwrap(), &[e]).is_ok());
    }
    #[test]
    fn changed_tmux_or_pane_identity_and_duplicate_evidence_refuse_attach() {
        let e = evidence("one.ts");
        let target = e.attach_target().unwrap();
        let mut changed = e.clone();
        changed.tmux_session = Some("replacement".into());
        assert!(validate_live_attach(&target, &[changed]).is_err());
        let mut changed = e.clone();
        changed.binding.pane_id += 1;
        assert!(validate_live_attach(&target, &[changed]).is_err());
        assert!(validate_live_attach(&target, &[e.clone(), e]).is_err());
    }
    fn history(host: &str) -> SessionRecord {
        SessionRecord::from_legacy(
            &AgentSessionRecord {
                agent: "codex".into(),
                session_id: "exact".into(),
                title: String::new(),
                cwd: "/synthetic".into(),
                host: Some(host.into()),
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: 1,
                last_user_message_at_unix_ms: None,
                status: "recent".into(),
                live_binding: None,
                resume_command: None,
                resume_unavailable_reason: None,
            },
            "local",
        )
    }
    #[test]
    fn resume_alias_without_exact_history_id_never_creates_attach_authority() {
        let mut catalog = SessionCatalog::from_discovery(AgentSessionDiscovery::default(), "local");
        enrich_live(&mut catalog, &[evidence("one.ts")]);
        assert!(catalog.sessions.is_empty());
    }
    #[test]
    fn same_name_remote_sessions_remain_host_scoped_and_ambiguous_bindings_fail_closed() {
        let mut catalog = SessionCatalog::from_discovery(AgentSessionDiscovery::default(), "local");
        let a = evidence("one.ts");
        let b = evidence("two.ts");
        catalog
            .sessions
            .extend([history("one.ts"), history("two.ts")]);
        enrich_live(&mut catalog, &[a.clone(), b.clone()]);
        assert_eq!(catalog.sessions.len(), 2);
        assert!(catalog.sessions.iter().all(|s| s.actions.len() == 1));
        assert!(validate_live_attach(&a.attach_target().unwrap(), &[b]).is_err());
        enrich_live(&mut catalog, &[a.clone(), a.clone()]);
        assert!(catalog.sessions.iter().all(|s| s.actions.is_empty()));
        assert!(validate_live_attach(&a.attach_target().unwrap(), &[]).is_err());
    }
}
