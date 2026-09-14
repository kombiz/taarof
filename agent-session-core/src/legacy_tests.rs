use crate::legacy::*;
#[cfg(feature = "opencode-history")]
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "taarof-agent-sessions-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir should be created");
    dir
}

#[test]
fn codex_discovery_reads_session_meta() {
    let dir = unique_temp_dir("codex");
    let session_path = dir.join("sessions/2026/05/20");
    fs::create_dir_all(&session_path).expect("codex session dir should exist");
    let file_path = session_path.join("rollout-2026-05-20T00-00-00-session-123.jsonl");
    fs::write(
            &file_path,
            "{\"timestamp\":\"2026-05-20T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-123\",\"cwd\":\"/tmp/project\"}}\n{\"timestamp\":\"2026-05-20T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Implement issue 133\"}]}}\n",
        )
        .expect("codex fixture should write");

    let (status, sessions) = discover_codex_sessions(Some(dir.join("sessions").as_path()), 10);
    assert!(status.ok);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "session-123");
    assert_eq!(sessions[0].cwd, "/tmp/project");
    assert_eq!(sessions[0].title, "Implement issue 133");
    assert_eq!(
        sessions[0].last_user_message_at_unix_ms,
        Some(1779235201000)
    );
}

#[test]
fn claude_discovery_skips_jsonl_without_session_metadata() {
    let dir = unique_temp_dir("claude-non-transcripts");
    let project = dir.join("projects/-tmp-project");
    let workflow = project.join("session-abc/subagents/workflows/wf_1");
    fs::create_dir_all(&workflow).expect("claude workflow dir should exist");
    fs::write(
        project.join("session-abc.jsonl"),
        "{\"type\":\"user\",\"sessionId\":\"session-abc\",\"cwd\":\"/tmp/project\",\"message\":{\"role\":\"user\",\"content\":\"Fix issue 6\"}}\n",
    )
    .expect("claude transcript should write");
    // Claude Code keeps non-transcript JSONL beside real sessions: workflow
    // journals carry no session metadata and bridge stubs carry no cwd.
    fs::write(
        workflow.join("journal.jsonl"),
        "{\"type\":\"launched\"}\n{\"type\":\"started\"}\n",
    )
    .expect("claude workflow journal should write");
    fs::write(
        project.join("bridge.jsonl"),
        "{\"type\":\"bridge-session\",\"sessionId\":\"bridge\"}\n",
    )
    .expect("claude bridge stub should write");

    let (status, sessions) = discover_claude_sessions(Some(dir.join("projects").as_path()), 10);
    assert!(status.ok, "non-transcript JSONL must not fail the provider");
    assert!(status.error.is_none());
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "session-abc");
}

#[test]
fn pi_discovery_prefers_session_map() {
    let dir = unique_temp_dir("pi");
    let sessions_dir = dir.join(".pi/agent/sessions/workspace");
    fs::create_dir_all(&sessions_dir).expect("pi session dir should exist");
    let session_file = sessions_dir.join("2026-05-20T00-00-00-000Z_pi-session.jsonl");
    fs::write(
            &session_file,
            "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-05-20T00:00:00Z\",\"cwd\":\"/tmp/pi\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Resume deploy work\"}]}}\n",
        )
        .expect("pi fixture should write");

    let session_map_path = dir.join(".pi/pi-acp");
    fs::create_dir_all(&session_map_path).expect("pi map dir should exist");
    fs::write(
            session_map_path.join("session-map.json"),
            format!(
                "{{\"version\":1,\"sessions\":{{\"pi-session\":{{\"sessionId\":\"pi-session\",\"cwd\":\"/tmp/pi\",\"sessionFile\":\"{}\",\"updatedAt\":\"2026-05-20T00:00:01Z\"}}}}}}",
                session_file.display()
            ),
        )
        .expect("pi session map should write");

    let (status, sessions) = discover_pi_sessions(
        Some(dir.join(".pi/agent/sessions").as_path()),
        Some(dir.join(".pi/pi-acp/session-map.json").as_path()),
        10,
    );
    assert!(status.ok);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "pi-session");
    assert_eq!(sessions[0].title, "Resume deploy work");
}

#[test]
fn kimi_discovery_reads_index_and_builds_verified_resume_command() {
    let dir = unique_temp_dir("kimi");
    let sessions_dir = dir.join(".kimi-code/sessions");
    let session_dir = sessions_dir.join("kimi-session");
    fs::create_dir_all(&session_dir).expect("kimi session dir should exist");
    fs::write(session_dir.join("state.json"), "{}").expect("kimi state should write");
    let index_path = dir.join(".kimi-code/session_index.jsonl");
    fs::write(
            &index_path,
            format!(
                "{{\"sessionId\":\"kimi-session\",\"sessionDir\":\"{}\",\"workDir\":\"/tmp/kimi project\"}}\n",
                session_dir.display()
            ),
        )
        .expect("kimi index should write");

    let (status, sessions) = discover_kimi_sessions(Some(&index_path), Some(&sessions_dir), 10);
    assert!(status.ok);
    assert!(status.history_available);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "kimi-session");
    assert_eq!(sessions[0].cwd, "/tmp/kimi project");
    assert_eq!(
        sessions[0].resume_command.as_deref(),
        Some("cd '/tmp/kimi project' && kimi --session kimi-session")
    );
    assert!(sessions[0].resume_unavailable_reason.is_none());
}

#[test]
fn most_recent_discovered_session_matches_normalized_agent_and_exact_cwd() {
    let record =
        |agent: &str, session_id: &str, cwd: &str, updated_at_unix_ms| AgentSessionRecord {
            agent: agent.into(),
            session_id: session_id.into(),
            title: "session".into(),
            cwd: cwd.into(),
            host: None,
            repo_root: None,
            started_at_unix_ms: None,
            updated_at_unix_ms,
            last_user_message_at_unix_ms: None,
            status: "recent".into(),
            live_binding: None,
            resume_command: None,
            resume_unavailable_reason: None,
        };
    let remote = |session_id: &str, cwd: &str, updated_at_unix_ms| AgentSessionRecord {
        host: Some("gpu-box".into()),
        ..record("codex", session_id, cwd, updated_at_unix_ms)
    };
    let discovery = AgentSessionDiscovery {
        providers: Vec::new(),
        sessions: vec![
            record("codex", "old", "/repo", 1),
            record("codex", "wrong-cwd", "/other", 3),
            record("codex", "new", "/repo", 2),
            // Same agent, same repo path, different host — the routine
            // collision that must never bind to a local pane.
            remote("remote-newest", "/repo", 99),
        ],
        remote_hosts: Vec::new(),
    };

    assert_eq!(
        most_recent_discovered_session(&discovery, "codex-cli", "/repo")
            .map(|session| session.session_id.as_str()),
        Some("new"),
        "a remote record must never resolve a local pane's session id"
    );
}

#[test]
fn kimi_discovery_missing_index_is_degraded_not_failed() {
    let dir = unique_temp_dir("kimi-missing");
    let (status, sessions) = discover_kimi_sessions(
        Some(&dir.join("missing.jsonl")),
        Some(&dir.join("sessions")),
        10,
    );
    assert!(status.ok);
    assert!(!status.history_available);
    assert!(status.warning.is_some());
    assert!(status.error.is_none());
    assert!(sessions.is_empty());
}

#[cfg(feature = "opencode-history")]
#[test]
fn opencode_discovery_reads_sqlite_rows() {
    let dir = unique_temp_dir("opencode");
    let db_path = dir.join("opencode.db");
    let connection = Connection::open(&db_path).expect("sqlite db should open");
    connection
            .execute_batch(
                "create table session (
                    id text primary key,
                    project_id text not null,
                    slug text not null,
                    directory text not null,
                    title text not null,
                    version text not null default '1',
                    share_url text,
                    summary_additions integer,
                    summary_deletions integer,
                    summary_files integer,
                    summary_diffs text,
                    revert text,
                    permission text,
                    time_created integer not null,
                    time_updated integer not null,
                    time_compacting integer,
                    time_archived integer,
                    workspace_id text,
                    path text,
                    agent text,
                    model text,
                    cost real default 0 not null,
                    tokens_input integer default 0 not null,
                    tokens_output integer default 0 not null,
                    tokens_reasoning integer default 0 not null,
                    tokens_cache_read integer default 0 not null,
                    tokens_cache_write integer default 0 not null
                );
                insert into session (id, project_id, slug, directory, title, version, time_created, time_updated)
                values ('ses_123', 'proj_1', 'steady-river', '/tmp/opencode', 'Issue 133', '1', 10, 20);",
            )
            .expect("opencode schema should write");

    drop(connection);

    let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);
    assert!(status.ok);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "ses_123");
    assert_eq!(sessions[0].title, "Issue 133");
}

#[cfg(feature = "opencode-history")]
#[test]
fn opencode_discovery_reports_degraded_when_db_missing() {
    let dir = unique_temp_dir("opencode-missing");
    let db_path = dir.join("opencode.db");

    let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);

    assert!(status.ok);
    assert!(!status.history_available);
    assert!(status.warning.is_some());
    assert!(status.error.is_none());
    assert_eq!(status.session_count, 0);
    assert!(sessions.is_empty());
}

#[cfg(feature = "opencode-history")]
#[test]
fn opencode_discovery_reports_error_on_unreadable_db() {
    let dir = unique_temp_dir("opencode-unreadable");
    let db_path = dir.join("opencode.db");
    fs::write(&db_path, b"this is not sqlite").expect("junk db should write");

    let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);

    assert!(!status.ok);
    assert!(!status.history_available);
    assert!(status.warning.is_none());
    assert!(status.error.is_some());
    assert_eq!(status.session_count, 0);
    assert!(sessions.is_empty());
}

#[cfg(not(feature = "opencode-history"))]
#[test]
fn opencode_adapter_absent_reports_unavailable() {
    let (status, sessions) = discover_opencode_sessions(None, 10);

    assert!(status.ok);
    assert!(!status.history_available);
    assert_eq!(
        status.warning.as_deref(),
        Some("OpenCode history support was not compiled into this build.")
    );
    assert!(status.error.is_none());
    assert_eq!(status.session_count, 0);
    assert!(sessions.is_empty());
}

#[test]
fn compact_title_truncates_on_character_boundaries() {
    let title = compact_title(&"🚀".repeat(80));

    assert!(title.ends_with("..."));
    assert_eq!(title.chars().count(), 72);
}

#[test]
fn shell_escape_handles_quotes() {
    assert_eq!(
        build_resume_command("claude", "/tmp/it's-here", "abc123"),
        "cd '/tmp/it'\"'\"'s-here' && claude --resume abc123"
    );
    assert_eq!(
        build_resume_command("custom; unsafe", "/tmp", "abc123"),
        "cd /tmp && 'custom; unsafe' abc123",
        "a persisted custom agent name must remain one shell word"
    );
}
