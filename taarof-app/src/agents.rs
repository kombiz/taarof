//! Agent discovery sidecar.
//!
//! The signature catalogue lives in [`signatures`], the
//! output-signal scanner in [`output`], the process-tree scanner in
//! [`scanner`], and the one canonical agent state machine in [`lifecycle`].

pub(crate) mod lifecycle;
mod output;
mod scanner;
mod signatures;
mod transcript;

#[cfg(test)]
use crate::workspace::AgentActivity;
use crate::workspace::AgentActivityState;

/// Classify whether an agent-state transition carries provider attribution.
/// Generic prompt waits still accept unattributed post-boundary evidence, but
/// surface it honestly as degraded instead of inventing provider certainty.
pub(crate) fn turn_evidence_quality(source: Option<&str>) -> &'static str {
    if source.is_some_and(|source| !source.trim().is_empty()) {
        "provider-attributed"
    } else {
        "degraded-generic"
    }
}

pub(crate) fn turn_state_label(state: AgentActivityState) -> &'static str {
    match state {
        AgentActivityState::Idle => "idle",
        AgentActivityState::Running => "running",
        AgentActivityState::WaitingInput => "waiting-input",
        AgentActivityState::Errored => "errored",
        AgentActivityState::Done => "done",
    }
}

pub(crate) fn turn_lifecycle_label(state: AgentLifecycle) -> &'static str {
    match state {
        AgentLifecycle::Idle => "idle",
        AgentLifecycle::Working => "running",
        AgentLifecycle::WaitingInput => "waiting-input",
        AgentLifecycle::Errored => "errored",
        AgentLifecycle::Done => "done",
    }
}

pub(crate) use lifecycle::{
    resolve as resolve_agent_lifecycle, strongest as strongest_agent_lifecycle, AgentLifecycle,
    PaneTurn, TurnMarker, TurnPhase,
};
pub use output::{
    format_agent_notification, scan_output_signal, summarize_activity_text, ScannedActivity,
};
pub use scanner::format_ports_label;
pub(crate) use scanner::{
    detect_agent_in_process_facts, detect_exact_agent_in_process_facts, get_child_pids,
    get_process_cmdline, get_process_comm, get_process_cwd, get_socket_inodes, is_ssh_process,
    try_build_listen_table, try_get_root_process_facts, ListenTableProbe, ProcessFact,
};
pub(crate) use transcript::{
    collect_transcript_bindings, resolve_touched_file_path, FileOp, ProviderNativeTurnId,
    TranscriptState, TranscriptTracker, TranscriptWorkEvent,
};

/// Provider-neutral evidence for a headless child agent discovered inside a
/// parent agent's native transcript. It deliberately carries no synthetic pane
/// identity: projection supplies the real owning tab/pane from the binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadlessAgentEvidence {
    pub stable_id: String,
    pub parent_id: String,
    pub provider: String,
    pub label: String,
    pub state: AgentLifecycle,
    pub activity: String,
    pub updated_at_unix_ms: u64,
}

/// One agent instance ready for a UI surface. Headless children inherit their
/// parent's real location and are never panes or independent focus targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentInstance {
    pub stable_id: String,
    pub parent_id: Option<String>,
    pub provider: String,
    pub label: String,
    pub state: AgentLifecycle,
    pub activity: String,
    pub tab_id: u32,
    pub pane_id: u32,
    pub headless: bool,
}
// Named only by tests (production code iterates `recent_files` without naming
// the element type); gate the re-export so non-test builds don't see it unused.
#[cfg(test)]
pub(crate) use transcript::TouchedFile;

/// Result of scanning a terminal's process tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentStatus {
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    pub running: bool,
}

/// Identifies the specific agent harness running in a terminal pane.
// Test-only classifier (not yet wired into production).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHarness {
    ClaudeCode,
    Codex,
    CopilotCli,
    Aider,
    OpenCode,
    Pi,
}

/// Normalised agent lifecycle state, independent of harness.
// Test-only classifier (not yet wired into production).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniversalAgentState {
    Unknown,
    Idle,
    Running,
    WaitingInput,
    Errored,
}

/// Map an agent name string to the corresponding [`AgentHarness`] variant.
// Test-only classifier (not yet wired into production).
#[cfg(test)]
pub fn detect_agent_harness(agent_name: Option<&str>) -> Option<AgentHarness> {
    let normalized = agent_name?.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "claude" | "claude-code" | "claude code" => Some(AgentHarness::ClaudeCode),
        "codex" | "codex-cli" | "codex cli" => Some(AgentHarness::Codex),
        "copilot" | "copilot-cli" | "copilot cli" => Some(AgentHarness::CopilotCli),
        "aider" => Some(AgentHarness::Aider),
        "opencode" | "open code" => Some(AgentHarness::OpenCode),
        "pi" | "pii" => Some(AgentHarness::Pi),
        _ => None,
    }
}

/// Derive a [`UniversalAgentState`] from harness identity and current activity.
// Test-only classifier (not yet wired into production).
#[cfg(test)]
pub fn classify_agent_state(
    agent_name: Option<&str>,
    activity: Option<&AgentActivity>,
) -> UniversalAgentState {
    match detect_agent_harness(agent_name) {
        Some(AgentHarness::ClaudeCode) => classify_claude_state(activity),
        Some(AgentHarness::Codex)
        | Some(AgentHarness::CopilotCli)
        | Some(AgentHarness::Aider)
        | Some(AgentHarness::OpenCode)
        | Some(AgentHarness::Pi) => UniversalAgentState::Unknown,
        None => classify_generic_state(activity),
    }
}

/// Returns `false` when the agent's classified state means it is stalled and
/// process-presence should not be counted as active work.
// Test-only classifier (not yet wired into production).
#[cfg(test)]
pub fn process_detection_counts_as_running(
    agent_name: Option<&str>,
    activity: Option<&AgentActivity>,
) -> bool {
    !matches!(
        classify_agent_state(agent_name, activity),
        UniversalAgentState::WaitingInput | UniversalAgentState::Errored
    )
}

// Test-only classifier (not yet wired into production).
#[cfg(test)]
fn classify_claude_state(activity: Option<&AgentActivity>) -> UniversalAgentState {
    match classify_generic_state(activity) {
        UniversalAgentState::Unknown | UniversalAgentState::Idle => UniversalAgentState::Idle,
        other => other,
    }
}

// Test-only classifier (not yet wired into production).
#[cfg(test)]
fn classify_generic_state(activity: Option<&AgentActivity>) -> UniversalAgentState {
    match activity.map(|activity| activity.state) {
        Some(AgentActivityState::Running) => UniversalAgentState::Running,
        Some(AgentActivityState::WaitingInput) => UniversalAgentState::WaitingInput,
        Some(AgentActivityState::Errored) => UniversalAgentState::Errored,
        Some(AgentActivityState::Idle | AgentActivityState::Done) => UniversalAgentState::Idle,
        None => UniversalAgentState::Idle,
    }
}

/// Stable badge metadata for an agent kind, surfaced in the sidebar, pane UI,
/// and the web Monitor so every surface renders the same identity chip.
///
/// This is the single source of truth for agent badges. The HTTP API embeds
/// these fields in each `agents[]` entry so web clients consume the same model
/// without maintaining a parallel table (see `docs/agent-state-model.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBadge {
    /// Normalised agent kind, e.g. `claude`, or the raw source for unknown kinds.
    pub name: String,
    /// Short uppercase chip label, e.g. `CLD`, `CDX`.
    pub short_label: String,
    /// Single short glyph (letter/emoji) for the compact chip.
    pub glyph: String,
    /// Stable colour token the UI maps to a colour class, e.g. `agent-claude`.
    /// Unknown kinds use the neutral `agent-generic` token.
    pub color_token: String,
    /// True when the kind was recognised; false for the generic fallback.
    pub known: bool,
}

/// Colour token used for unrecognised agents.
pub const GENERIC_AGENT_COLOR_TOKEN: &str = "agent-generic";

/// Resolve a badge for an agent name.
///
/// Matching is case-insensitive on the normalised kind (trimmed, lowercased).
/// Known kinds map to stable `(short_label, glyph, color_token)` triples. An
/// unrecognised or empty name degrades to a neutral generic badge that still
/// carries the detected source label (uppercased, truncated) so the operator
/// sees *something* identifying for a novel agent.
pub fn agent_badge(agent_name: &str) -> AgentBadge {
    let normalized = agent_name.trim().to_ascii_lowercase();
    // (kind, short_label, glyph, color_token)
    let known: Option<(&str, &str, &str, &str)> = match normalized.as_str() {
        "claude" | "claude-code" | "claude code" => {
            Some(("claude", "CLD", "\u{2726}", "agent-claude"))
        }
        "codex" | "codex-cli" | "codex cli" => Some(("codex", "CDX", "\u{25c8}", "agent-codex")),
        "aider" => Some(("aider", "AID", "A", "agent-aider")),
        "copilot" | "copilot-cli" | "copilot cli" => {
            Some(("copilot", "CPL", "\u{2708}", "agent-copilot"))
        }
        "opencode" | "open code" => Some(("opencode", "OPC", "\u{25d0}", "agent-opencode")),
        "pi" | "pii" => Some(("pi", "PI", "\u{03c0}", "agent-pi")),
        "cursor" => Some(("cursor", "CUR", "\u{2038}", "agent-cursor")),
        "cline" => Some(("cline", "CLN", "C", "agent-cline")),
        "continue" => Some(("continue", "CNT", "\u{25b6}", "agent-continue")),
        "goose" => Some(("goose", "GSE", "G", "agent-goose")),
        "gemini" => Some(("gemini", "GEM", "\u{25c6}", "agent-gemini")),
        "kimi" => Some(("kimi", "KMI", "K", "agent-kimi")),
        _ => None,
    };

    match known {
        Some((kind, short_label, glyph, color_token)) => AgentBadge {
            name: kind.to_string(),
            short_label: short_label.to_string(),
            glyph: glyph.to_string(),
            color_token: color_token.to_string(),
            known: true,
        },
        None => {
            let source = agent_name.trim();
            let short_label = if source.is_empty() {
                "AGENT".to_string()
            } else {
                source
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .take(4)
                    .collect::<String>()
                    .to_ascii_uppercase()
            };
            let name = if source.is_empty() {
                "agent".to_string()
            } else {
                normalized
            };
            AgentBadge {
                name,
                short_label,
                glyph: "\u{2022}".to_string(),
                color_token: GENERIC_AGENT_COLOR_TOKEN.to_string(),
                known: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // The test module pulls in items from the parent module's `pub use`
    // re-exports below. Keep this block positioned after the items it
    // references (AgentStatus, UniversalAgentState, etc.) so `super::*`
    // resolves correctly.
    use super::output::{
        scan_output_activity, scan_output_signal, summarize_activity_text, ScannedActivity,
    };
    use super::scanner::extract_agent_session_id;
    use super::scanner::match_agent_signature;
    use super::scanner::{
        collect_process_tree, format_ports_label, parse_tcp_listen_line, parse_udp_listen_line,
        try_build_listen_table,
    };
    use super::signatures::{
        default_agent_signatures, load_agent_signatures_from_path, merged_agent_signatures_for_cwd,
    };
    use super::*;
    use crate::workspace::{AgentActivity, AgentActivityOrigin, AgentActivityState};
    use std::time::Instant;

    fn activity(state: AgentActivityState, text: &str) -> AgentActivity {
        AgentActivity {
            state,
            text: text.to_string(),
            source: Some("claude".to_string()),
            origin: AgentActivityOrigin::Termprop,
            updated_at: Instant::now(),
        }
    }

    #[test]
    fn test_parse_tcp_listen_line_valid() {
        let line = "   0: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let result = parse_tcp_listen_line(line);
        assert_eq!(result, Some((12345, 3000)));
    }

    #[test]
    fn test_parse_tcp_listen_line_not_listening() {
        let line = "   1: 0100007F:C350 0100007F:0BB8 01 00000000:00000000 00:00000000 00000000  1000        0 67890 1 0000000000000000 20 4 30 10 -1";
        assert_eq!(parse_tcp_listen_line(line), None);
    }

    #[test]
    fn test_parse_tcp_listen_line_header() {
        let line = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";
        assert_eq!(parse_tcp_listen_line(line), None);
    }

    #[test]
    fn test_parse_tcp_listen_line_port_80() {
        let line = "   2: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99999 1 0000000000000000 100 0 0 10 0";
        let result = parse_tcp_listen_line(line);
        assert_eq!(result, Some((99999, 80)));
    }

    // #63: UDP listener detection
    #[test]
    fn test_parse_udp_listen_line_unconnected_listener() {
        // State 07 with zero remote address = unconnected UDP listener (port 5353 = 0x14E9)
        let line = "   0: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 23781 2 0000000000000000 0";
        let result = parse_udp_listen_line(line);
        assert_eq!(result, Some((23781, 5353)));
    }

    #[test]
    fn test_parse_udp_listen_line_connected_returns_none() {
        // State 01 = connected UDP socket, not a listener
        let line = "  20: 6901A8C0:B9A8 D2776034:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 102208995 2 0000000000000000 0";
        assert_eq!(parse_udp_listen_line(line), None);
    }

    #[test]
    fn test_parse_udp_listen_line_header_returns_none() {
        let line = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops";
        assert_eq!(parse_udp_listen_line(line), None);
    }

    #[test]
    fn test_parse_udp_listen_line_port_53() {
        // DNS listener: port 53 = 0x0035
        let line = "   1: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 12345 2 0000000000000000 0";
        let result = parse_udp_listen_line(line);
        assert_eq!(result, Some((12345, 53)));
    }

    #[test]
    fn test_collect_process_tree_includes_self() {
        let pid = std::process::id() as i32;
        let tree = collect_process_tree(pid);
        assert!(tree.contains(&pid), "tree should include the root PID");
    }

    #[test]
    fn test_build_listen_table_does_not_panic() {
        let _table = try_build_listen_table();
    }

    #[test]
    fn test_format_ports_label_empty() {
        assert_eq!(format_ports_label(&[]), "");
    }

    #[test]
    fn test_format_ports_label_single() {
        assert_eq!(format_ports_label(&[3000]), ":3000");
    }

    #[test]
    fn test_format_ports_label_multiple() {
        assert_eq!(format_ports_label(&[3000, 8080]), ":3000 :8080");
    }

    #[test]
    fn test_format_ports_label_overflow() {
        let ports = vec![80, 443, 3000, 5432, 6379, 8080, 9090];
        assert_eq!(format_ports_label(&ports), ":80 :443 :3000 :5432 :6379 +2");
    }

    #[test]
    fn test_summarize_activity_text_normalizes_tool_events() {
        assert_eq!(
            summarize_activity_text("Read /tmp/user/project/src/main.rs"),
            "reading main.rs"
        );
        assert_eq!(
            summarize_activity_text("Write /tmp/user/project/src/sidebar.rs"),
            "editing sidebar.rs"
        );
        assert_eq!(
            summarize_activity_text("Bash: npm test -- --watch=false"),
            "running npm test --..."
        );
    }

    // #25: Read must render as "reading <basename>"; Edit/Write/Update as "editing <basename>".
    // This test locks the distinction so refactors cannot accidentally collapse the two verbs.
    #[test]
    fn test_read_renders_as_reading_not_editing() {
        assert_eq!(
            summarize_activity_text("Read /tmp/user/foo/bar.rs"),
            "reading bar.rs"
        );
        // Case-insensitive
        assert_eq!(
            summarize_activity_text("read /path/to/config.toml"),
            "reading config.toml"
        );
    }

    #[test]
    fn test_edit_write_update_render_as_editing() {
        assert_eq!(
            summarize_activity_text("Edit /tmp/user/foo/bar.rs"),
            "editing bar.rs"
        );
        assert_eq!(
            summarize_activity_text("Write /tmp/user/foo/bar.rs"),
            "editing bar.rs"
        );
        assert_eq!(
            summarize_activity_text("Update /tmp/user/foo/bar.rs"),
            "editing bar.rs"
        );
    }

    #[test]
    fn test_summarize_activity_text_normalizes_waiting_text() {
        assert_eq!(
            summarize_activity_text("Waiting for user input"),
            "waiting for input"
        );
    }

    #[test]
    fn test_summarize_activity_text_truncates_unknown_messages() {
        assert_eq!(
            summarize_activity_text("Editing a very long file name that will not fit"),
            "editing a very long ..."
        );
    }

    #[test]
    fn test_summarize_activity_text_normalizes_search_and_fetch() {
        assert_eq!(
            summarize_activity_text("Search src/**/*.rs"),
            "searching src/**/*.rs"
        );
        assert_eq!(
            summarize_activity_text("Fetch https://example.com/docs"),
            "fetching https://exa ..."
        );
    }

    #[test]
    fn test_scan_output_activity_prefers_latest_matching_line() {
        let output = "\
thinking\n\
Read(/tmp/first.rs)\n\
Bash: cargo test -p taarof-app\n";
        assert_eq!(
            scan_output_activity(output),
            Some("running cargo test ...".to_string())
        );
    }

    #[test]
    fn test_scan_output_activity_detects_waiting_prompt() {
        let output = "\
all done\n\
Claude needs your permission to use Bash\n";
        assert_eq!(
            scan_output_activity(output),
            Some("waiting for input".to_string())
        );
    }

    #[test]
    fn test_scan_output_signal_marks_waiting_input() {
        let output = "\
all done\n\
Claude needs your permission to use Bash\n";
        assert_eq!(
            scan_output_signal(output),
            Some(ScannedActivity {
                state: AgentActivityState::WaitingInput,
                summary: "waiting for input".to_string(),
            })
        );
    }

    #[test]
    fn test_scan_output_signal_detects_claude_question_selector() {
        let output = "\
● User answered Claude's questions:
  ⎿  · How should the model selection persist? → Remembered in browser (Recommended)

╭ Roles exposed
│ Which roles should get their own model dropdown in the app?
│
│ 1. Orchestrator + Fast + Search (Recommended)
│ 2. Orchestrator + Fast only
│ 3. Single LLM model + profile
╰────────────────

Enter to select · ↑/↓ to navigate · Esc to cancel";

        assert_eq!(
            scan_output_signal(output),
            Some(ScannedActivity {
                state: AgentActivityState::WaitingInput,
                summary: "waiting for input".to_string(),
            })
        );
    }

    #[test]
    fn test_match_agent_signature_uses_process_name() {
        let signatures = default_agent_signatures();
        assert_eq!(
            match_agent_signature(Some("codex"), None, &signatures),
            Some("codex".to_string())
        );
    }

    #[test]
    fn test_match_agent_signature_uses_cmdline() {
        let signatures = vec![super::signatures::AgentSignature {
            name: "custom".into(),
            patterns: vec!["my-custom-agent".into()],
        }];
        let cmdline = vec!["python".to_string(), "my-custom-agent".to_string()];
        assert_eq!(
            match_agent_signature(Some("python"), Some(&cmdline), &signatures),
            Some("custom".to_string())
        );
    }

    #[test]
    fn test_load_agent_signatures_from_path() {
        let dir = std::env::temp_dir().join(format!("taarof-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-signatures.json");
        std::fs::write(
            &path,
            r#"{"signatures":[{"name":"helper","patterns":["helper-agent","helperd"]}]}"#,
        )
        .unwrap();
        let loaded = load_agent_signatures_from_path(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "helper");
        assert_eq!(loaded[0].patterns, vec!["helper-agent", "helperd"]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn claude_waiting_input_overrides_process_presence() {
        let activity = activity(AgentActivityState::WaitingInput, "waiting for input");
        assert_eq!(
            classify_agent_state(Some("claude"), Some(&activity)),
            UniversalAgentState::WaitingInput
        );
        assert!(!process_detection_counts_as_running(
            Some("claude"),
            Some(&activity)
        ));
    }

    #[test]
    fn non_claude_harnesses_stub_to_unknown() {
        for agent_name in ["codex", "copilot", "aider", "opencode", "pi"] {
            assert_eq!(
                detect_agent_harness(Some(agent_name)),
                Some(match agent_name {
                    "codex" => AgentHarness::Codex,
                    "copilot" => AgentHarness::CopilotCli,
                    "aider" => AgentHarness::Aider,
                    "opencode" => AgentHarness::OpenCode,
                    "pi" => AgentHarness::Pi,
                    _ => unreachable!(),
                })
            );
            assert_eq!(
                classify_agent_state(Some(agent_name), None),
                UniversalAgentState::Unknown
            );
        }
    }

    #[test]
    fn builtin_pi_signature_matches_exact_binary_name() {
        let signatures = default_agent_signatures();
        assert_eq!(
            match_agent_signature(Some("pi"), None, &signatures),
            Some("pi".to_string())
        );
        let cmdline = vec!["/usr/local/bin/pii".to_string()];
        assert_eq!(
            match_agent_signature(Some("bash"), Some(&cmdline), &signatures),
            Some("pi".to_string())
        );
    }

    #[test]
    fn extract_agent_session_id_supports_resume_and_session_flags() {
        assert_eq!(
            extract_agent_session_id(
                "codex",
                &[
                    "codex".to_string(),
                    "resume".to_string(),
                    "abc123".to_string()
                ]
            ),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_agent_session_id(
                "claude",
                &["claude".to_string(), "--resume=session-456".to_string()]
            ),
            Some("session-456".to_string())
        );
        assert_eq!(
            extract_agent_session_id(
                "pi",
                &[
                    "pi".to_string(),
                    "--session".to_string(),
                    "session-789".to_string()
                ]
            ),
            Some("session-789".to_string())
        );
        assert_eq!(
            extract_agent_session_id(
                "kimi",
                &[
                    "kimi".to_string(),
                    "-S".to_string(),
                    "kimi-session".to_string()
                ]
            ),
            Some("kimi-session".to_string())
        );
        assert_eq!(
            extract_agent_session_id("kimi", &["kimi".to_string(), "-S=kimi-equals".to_string()]),
            Some("kimi-equals".to_string())
        );
        assert_eq!(
            extract_agent_session_id(
                "claude",
                &["claude".to_string(), "-S".to_string(), "style".to_string()]
            ),
            None,
            "Kimi's short flag must not create false session ids for other agents"
        );
    }

    #[test]
    fn test_project_config_overrides_agent_signatures() {
        let repo_root =
            std::env::temp_dir().join(format!("taarof-project-signatures-{}", std::process::id()));
        let nested = repo_root.join("pkg/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        std::fs::write(
            repo_root.join(".taarof.config"),
            r#"
[agents]
signatures = [{ name = "helper", patterns = ["helper-agent"] }]
"#,
        )
        .unwrap();

        let signatures = merged_agent_signatures_for_cwd(Some(nested.to_str().unwrap()));
        let cmdline = vec!["python".to_string(), "helper-agent".to_string()];
        assert_eq!(
            match_agent_signature(Some("python"), Some(&cmdline), &signatures),
            Some("helper".to_string())
        );

        let _ = std::fs::remove_dir_all(repo_root);
    }

    #[test]
    fn agent_badge_resolves_known_kinds_with_stable_fields() {
        // (input, expected name, short_label, glyph, color_token)
        let cases = [
            ("claude", "claude", "CLD", "\u{2726}", "agent-claude"),
            ("Claude", "claude", "CLD", "\u{2726}", "agent-claude"),
            ("claude-code", "claude", "CLD", "\u{2726}", "agent-claude"),
            ("codex", "codex", "CDX", "\u{25c8}", "agent-codex"),
            ("aider", "aider", "AID", "A", "agent-aider"),
            ("copilot", "copilot", "CPL", "\u{2708}", "agent-copilot"),
            ("opencode", "opencode", "OPC", "\u{25d0}", "agent-opencode"),
            ("pi", "pi", "PI", "\u{03c0}", "agent-pi"),
            ("pii", "pi", "PI", "\u{03c0}", "agent-pi"),
            ("cursor", "cursor", "CUR", "\u{2038}", "agent-cursor"),
            ("cline", "cline", "CLN", "C", "agent-cline"),
            ("continue", "continue", "CNT", "\u{25b6}", "agent-continue"),
            ("goose", "goose", "GSE", "G", "agent-goose"),
            ("gemini", "gemini", "GEM", "\u{25c6}", "agent-gemini"),
            ("kimi", "kimi", "KMI", "K", "agent-kimi"),
        ];
        for (input, name, short_label, glyph, color_token) in cases {
            let badge = agent_badge(input);
            assert!(badge.known, "{input} should resolve to a known badge");
            assert_eq!(badge.name, name, "name for {input}");
            assert_eq!(badge.short_label, short_label, "short_label for {input}");
            assert_eq!(badge.glyph, glyph, "glyph for {input}");
            assert_eq!(badge.color_token, color_token, "color_token for {input}");
        }
    }

    #[test]
    fn agent_badge_falls_back_to_generic_carrying_source_label() {
        let badge = agent_badge("MysteryBot");
        assert!(!badge.known);
        assert_eq!(badge.name, "mysterybot");
        // Source label is uppercased and truncated to a short chip.
        assert_eq!(badge.short_label, "MYST");
        assert_eq!(badge.glyph, "\u{2022}");
        assert_eq!(badge.color_token, GENERIC_AGENT_COLOR_TOKEN);
    }

    #[test]
    fn agent_badge_generic_handles_empty_and_whitespace_names() {
        for input in ["", "   "] {
            let badge = agent_badge(input);
            assert!(!badge.known);
            assert_eq!(badge.name, "agent");
            assert_eq!(badge.short_label, "AGENT");
            assert_eq!(badge.glyph, "\u{2022}");
            assert_eq!(badge.color_token, GENERIC_AGENT_COLOR_TOKEN);
        }
    }
}
