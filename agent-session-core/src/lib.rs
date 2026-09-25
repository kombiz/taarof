//! Shared local agent-session discovery and versioned action contract.
mod live;
mod message_time;
mod repository;
pub use live::*;
pub use repository::repository_common_dir;
pub mod adapters;
mod cache;
mod contract;
pub mod legacy;
pub mod protocol;
pub use cache::DiscoveryCache;
pub use contract::*;
pub use legacy::{
    AgentSessionDiscovery, AgentSessionProviderStatus, AgentSessionRecord, AgentSessionsSnapshot,
    DiscoveryRoots, LiveAgentBinding, RemoteHostStatus,
};
#[cfg(test)]
mod legacy_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuity_actions_name_their_distinct_authorities() {
        assert_eq!(ActionKind::Attach.label(), "Reattach live terminal");
        assert_eq!(ActionKind::Resume.label(), "Resume agent conversation");
        assert!(ActionKind::Attach
            .authority_description()
            .contains("live process target"));
        assert!(ActionKind::Resume
            .authority_description()
            .contains("provider session identity"));
        assert!(!ActionKind::New.authority_description().contains("resume"));
    }
    use serde_json::json;
    use std::path::PathBuf;

    fn record(provider: &str, id: &str) -> AgentSessionRecord {
        AgentSessionRecord {
            agent: provider.into(),
            session_id: id.into(),
            title: "Fixture title".into(),
            cwd: "/synthetic/project".into(),
            host: None,
            repo_root: None,
            started_at_unix_ms: Some(1),
            updated_at_unix_ms: 2,
            last_user_message_at_unix_ms: None,
            status: "recent".into(),
            live_binding: None,
            resume_command: Some("DO NOT EXECUTE OR PARSE".into()),
            resume_unavailable_reason: None,
        }
    }

    #[test]
    fn stable_ref_scopes_opaque_id_by_provider_and_host() {
        let a = StableRef::new("codex", "BUILD.TS.", "opaque:id/with spaces");
        assert_eq!(
            a,
            StableRef::new("codex", "build.ts", "opaque:id/with spaces")
        );
        assert_ne!(
            a,
            StableRef::new("claude", "build.ts", "opaque:id/with spaces")
        );
        assert_ne!(
            a,
            StableRef::new("codex", "other.ts", "opaque:id/with spaces")
        );
        assert_ne!(
            a,
            StableRef::new("codex", "build.ts", "OPAQUE:id/with spaces")
        );
        let original = record("codex", "opaque:id/with spaces");
        let mut changed = original.clone();
        changed.cwd = "/moved".into();
        changed.title = "Renamed".into();
        assert_eq!(
            SessionRecord::from_legacy(&original, "build.ts").stable_ref,
            SessionRecord::from_legacy(&changed, "build.ts").stable_ref
        );
    }

    #[test]
    fn structured_action_preserves_argv_without_shell_roundtrip() {
        let mut input = record("codex", "id with 'quotes' \"double\" ; $(false)");
        input.cwd = "/synthetic/it's a project".into();
        let session = SessionRecord::from_legacy(&input, "local.ts");
        let plan = &session.actions[0];
        assert_eq!(plan.kind, ActionKind::Resume);
        assert_eq!(plan.transport, Transport::Local);
        assert_eq!(plan.program, "codex");
        assert_eq!(plan.argv, vec!["resume", &input.session_id]);
        assert_eq!(plan.cwd, PathBuf::from(&input.cwd));
        assert_eq!(plan.confirmation, Confirmation::Required);
        assert!(plan.remote.is_none());
        assert!(!serde_json::to_string(plan)
            .unwrap()
            .contains("DO NOT EXECUTE"));
        let mut remote = input.clone();
        remote.host = Some("remote.ts".into());
        let remote = SessionRecord::from_legacy(&remote, "local.ts");
        assert!(
            remote.actions.is_empty(),
            "host label alone is not SSH authority"
        );
    }

    #[test]
    fn catalog_merges_only_exact_provider_host_session_identity() {
        let mut older = record("codex", "same");
        older.updated_at_unix_ms = 1;
        let mut newest = older.clone();
        newest.updated_at_unix_ms = 10;
        newest.cwd = "/moved".into();
        let different_id = record("codex", "different");
        let different_provider = record("claude", "same");
        let mut different_host = older.clone();
        different_host.host = Some("remote.ts".into());
        let mut active = record("pi", "active");
        active.status = "active".into();
        let catalog = SessionCatalog::from_discovery(
            AgentSessionDiscovery {
                sessions: vec![
                    older,
                    newest,
                    different_id,
                    different_provider,
                    different_host,
                    active,
                ],
                ..Default::default()
            },
            "local.ts",
        );
        assert_eq!(catalog.sessions.len(), 5);
        assert_eq!(catalog.sessions[0].stable_ref.provider_id, "pi");
        assert_eq!(catalog.sessions[1].updated_at_unix_ms, 10);
        assert_eq!(catalog.sessions[1].display.cwd, "/moved");
    }

    #[test]
    fn display_fields_strip_terminal_and_bidi_controls() {
        let hostile = format!(
            "\x1b[31mred\x1b[0m\x1b]0;hidden\x07\r\n\t\u{202e}\u{2066}{}",
            "界".repeat(2000)
        );
        let mut input = record(&hostile, &hostile);
        input.title = hostile.clone();
        input.cwd = hostile.clone();
        input.host = Some(hostile.clone());
        input.repo_root = Some(hostile.clone());
        input.resume_unavailable_reason = Some(hostile.clone());
        let normalized = SessionRecord::from_legacy(&input, "local.ts");
        let display = serde_json::to_value(&normalized.display).unwrap();
        for value in display
            .as_object()
            .unwrap()
            .values()
            .filter_map(|v| v.as_str())
        {
            assert!(value.chars().count() <= DISPLAY_FIELD_LIMIT);
            assert!(!value
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{202e}' | '\u{2066}')));
            assert!(!value.contains("hidden"));
            assert!(!value.contains("[31m"));
        }
        for warning in &normalized.warnings {
            assert!(warning.chars().count() <= DISPLAY_FIELD_LIMIT);
            assert!(!warning.contains('\u{202e}'));
        }
        assert_eq!(
            normalized.stable_ref.session_id, hostile,
            "opaque identity is not display text"
        );
    }

    #[test]
    fn all_builtin_adapters_pass_shared_contract_fixtures() {
        let home = std::env::temp_dir().join(format!("agent-core-contract-{}", std::process::id()));
        let write = |path: &str, value: serde_json::Value| {
            let path = home.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value.to_string()).unwrap();
        };
        write(
            ".claude/projects/test/one.jsonl",
            json!({"sessionId":"claude-id","cwd":"/synthetic/project"}),
        );
        write(
            ".codex/sessions/one.jsonl",
            json!({"type":"session_meta","payload":{"id":"codex-id","cwd":"/synthetic/project"}}),
        );
        write(
            ".pi/agent/sessions/one.jsonl",
            json!({"type":"session","id":"pi-id","cwd":"/synthetic/project"}),
        );
        write(
            ".kimi-code/session_index.jsonl",
            json!({"sessionId":"kimi-id","sessionDir":"kimi-id","workDir":"/synthetic/project"}),
        );
        #[cfg(feature = "opencode-history")]
        {
            let path = home.join(".local/share/opencode/opencode.db");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = rusqlite::Connection::open(path).unwrap();
            db.execute_batch("create table session (id text, directory text, title text, time_created integer, time_updated integer); insert into session values ('opencode-id','/synthetic/project','Fixture title',1,2);").unwrap();
        }
        let registry = BuiltinRegistry::new(DiscoveryRoots::from_home(Some(home.clone())));
        assert_eq!(registry.adapters().len(), 6);
        for adapter in registry.adapters() {
            let metadata = adapter.metadata();
            assert_eq!(metadata.schema, "agent.provider.v1");
            let (status, sessions) = adapter.discover();
            assert!(status.ok, "{}", metadata.id);
            assert_eq!(adapter.probe(), status);
            let expected = metadata.id != "copilot"
                && (metadata.id != "opencode" || cfg!(feature = "opencode-history"));
            assert_eq!(sessions.len(), usize::from(expected), "{}", metadata.id);
            assert_eq!(status.session_count, sessions.len());
            let new = adapter
                .plan_new(PathBuf::from("/synthetic/project"))
                .unwrap();
            assert_eq!(new.kind, ActionKind::New);
            assert!(new.argv.is_empty());
            for session in sessions {
                assert_eq!(session.session_id, format!("{}-id", metadata.id));
                assert_eq!(session.cwd, "/synthetic/project");
                let planned = adapter.plan_resume(&session).unwrap();
                assert_eq!(
                    planned,
                    SessionRecord::from_legacy(&session, "local.ts").actions[0]
                );
            }
        }
        // A malformed provider store must not erase another provider's success.
        write(".pi/pi-acp/session-map.json", json!("malformed map"));
        std::fs::write(home.join(".pi/pi-acp/session-map.json"), "{").unwrap();
        let discovery = registry.discover();
        assert!(
            !discovery
                .providers
                .iter()
                .find(|p| p.name == "pi")
                .unwrap()
                .ok
        );
        assert!(discovery.sessions.iter().any(|s| s.agent == "codex"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn remote_plans_use_explicit_target_and_keep_degraded_truth() {
        let mut input = record("codex", "opaque 'remote' id");
        input.host = Some("remote.ts".into());
        let catalog = SessionCatalog::from_discovery(
            AgentSessionDiscovery {
                sessions: vec![input],
                remote_hosts: vec![RemoteHostStatus {
                    host: "remote.ts".into(),
                    ssh_target: "user@remote.ts".into(),
                    ok: false,
                    stale: true,
                    error: Some("offline".into()),
                    session_count: 1,
                    dropped_lines: 0,
                    truncated_files: 0,
                    warning: None,
                    observed_at_unix_ms: Some(1),
                }],
                ..Default::default()
            },
            "local.ts",
        );
        let session = &catalog.sessions[0];
        assert_eq!(session.source, SessionSource::RemoteHistory);
        assert_eq!(session.actions[0].transport, Transport::Ssh);
        assert_eq!(
            session.actions[0].remote.as_ref().unwrap().ssh_target,
            "user@remote.ts"
        );
        assert_eq!(session.actions[0].argv, ["resume", "opaque 'remote' id"]);
        assert!(session.warnings.iter().any(|w| w == "offline"));
        assert!(catalog.remote_hosts[0].stale);
    }

    #[test]
    fn cwd_live_hint_is_not_exact_live_authority() {
        let mut input = record("codex", "one");
        input.status = "active".into();
        input.live_binding = Some(LiveAgentBinding {
            agent: "codex".into(),
            session_id: Some("another".into()),
            cwd: Some(input.cwd.clone()),
            workspace_id: 1,
            workspace_name: "workspace".into(),
            tab_id: 2,
            tab_name: "tab".into(),
            pane_id: 3,
        });
        let normalized = SessionRecord::from_legacy(&input, "local.ts");
        assert!(normalized.live_binding.is_none());
        assert_eq!(normalized.state, SessionState::Recent);
        input.live_binding.as_mut().unwrap().session_id = Some("one".into());
        let exact = SessionRecord::from_legacy(&input, "local.ts");
        assert_eq!(exact.confidence, Confidence::ExactLiveBinding);
        assert_eq!(exact.state, SessionState::Active);
    }

    #[test]
    fn v2_never_projects_legacy_prompt_titles() {
        let mut input = record("codex", "opaque-id");
        input.title = "PRIVATE_PROMPT_SENTINEL".into();
        let v2 = SessionRecord::from_legacy(&input, "local.ts");
        assert!(!serde_json::to_string(&v2)
            .unwrap()
            .contains("PRIVATE_PROMPT_SENTINEL"));
        assert!(v2.display.title.contains("opaque-id"));
        assert_eq!(input.title, "PRIVATE_PROMPT_SENTINEL");
    }

    #[test]
    fn remote_identity_survives_display_alias_rename() {
        let make = |alias: &str| {
            let mut input = record("codex", "one");
            input.host = Some(alias.into());
            SessionCatalog::from_discovery(
                AgentSessionDiscovery {
                    sessions: vec![input],
                    remote_hosts: vec![RemoteHostStatus {
                        host: alias.into(),
                        ssh_target: "user@remote.ts".into(),
                        ok: true,
                        stale: false,
                        error: None,
                        session_count: 1,
                        dropped_lines: 0,
                        truncated_files: 0,
                        warning: None,
                        observed_at_unix_ms: Some(1),
                    }],
                    ..Default::default()
                },
                "local.ts",
            )
            .sessions
            .remove(0)
        };
        assert_ne!(
            StableRef::new("codex", "Alice@remote.ts", "one"),
            StableRef::new("codex", "alice@remote.ts", "one")
        );
        assert_eq!(
            StableRef::new("codex", "Alice@REMOTE.TS.", "one"),
            StableRef::new("codex", "Alice@remote.ts", "one")
        );
        assert_eq!(
            make("old label").stable_ref,
            make("renamed label").stable_ref
        );
    }

    #[test]
    fn cache_preserves_discovery_and_expires_without_scanning() {
        let cache = DiscoveryCache::new(std::time::Duration::from_secs(20));
        assert!(cache.get().is_none());
        let discovery = AgentSessionDiscovery {
            sessions: vec![record("pi", "one")],
            ..Default::default()
        };
        cache.store(discovery.clone());
        assert_eq!(cache.get(), Some(discovery.clone()));
        let expired = DiscoveryCache::new(std::time::Duration::ZERO);
        expired.store(discovery);
        assert!(expired.get().is_none());
    }

    #[test]
    fn taarof_v1_projection_remains_compatible() {
        let input = record("codex", "id");
        let snapshot = AgentSessionsSnapshot {
            schema: "taarof.agent-sessions.v1",
            generated_at_unix_ms: 42,
            providers: vec![],
            sessions: vec![input.clone()],
            remote_hosts: vec![],
        };
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "schema":"taarof.agent-sessions.v1", "generated_at_unix_ms":42, "providers":[],
                "sessions":[{"agent":"codex","session_id":"id","title":"Fixture title",
                    "cwd":"/synthetic/project","started_at_unix_ms":1,"updated_at_unix_ms":2,
                    "status":"recent","resume_command":"DO NOT EXECUTE OR PARSE"}]
            })
        );
        let v2 = SessionCatalog::from_discovery(
            AgentSessionDiscovery {
                sessions: snapshot.sessions.clone(),
                ..Default::default()
            },
            "local.ts",
        );
        assert_eq!(v2.schema, "agent.sessions.v2");
        assert_eq!(v2.sessions[0].actions[0].argv, ["resume", "id"]);
        assert_eq!(v2.sessions[0].source, SessionSource::LocalHistory);
        assert_eq!(v2.sessions[0].confidence, Confidence::ProviderIdentity);
        assert_eq!(snapshot.sessions, vec![input]);
    }
}
