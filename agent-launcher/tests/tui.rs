use agent_launcher::tui::{NewProvider, Picker, RowKey};
use agent_session_core::*;
fn session(id: &str, time: u64, active: bool) -> SessionRecord {
    SessionRecord::from_legacy(
        &AgentSessionRecord {
            agent: "fixture".into(),
            session_id: id.into(),
            title: "inert metadata".into(),
            cwd: "/tmp/project".into(),
            host: None,
            repo_root: Some("/tmp/project".into()),
            started_at_unix_ms: None,
            updated_at_unix_ms: time,
            last_user_message_at_unix_ms: None,
            status: if active { "active" } else { "recent" }.into(),
            live_binding: None,
            resume_command: None,
            resume_unavailable_reason: None,
        },
        "local.ts",
    )
}
fn catalog(sessions: Vec<SessionRecord>) -> SessionCatalog {
    SessionCatalog {
        schema: "agent.sessions.v2",
        providers: vec![],
        sessions,
        remote_hosts: vec![],
    }
}
fn new_provider() -> NewProvider {
    NewProvider {
        id: "fixture".into(),
        plan: ActionPlan {
            attach: None,
            kind: ActionKind::New,
            transport: Transport::Local,
            program: "fixture".into(),
            argv: vec![],
            cwd: "/tmp".into(),
            remote: None,
            confirmation: Confirmation::Required,
        },
    }
}
#[test]
fn tui_picker_groups_and_sorts_normalized_rows() {
    let picker = Picker::new(
        catalog(vec![
            session("old", 1, false),
            session("fresh", 20, false),
            session("live", 2, true),
        ]),
        vec![new_provider()],
        "local.ts".into(),
    );
    let rows = picker.rows();
    assert_eq!(rows[0].key, RowKey::New("fixture".into()));
    // Unverified active labels must not become live attach rows.
    assert!(rows.iter().all(|r| r.section != "Active"));
    assert_eq!(
        rows[1].key,
        RowKey::Session(StableRef::new("fixture", "local.ts", "fresh"))
    );
    assert_eq!(
        rows[3].key,
        RowKey::Session(StableRef::new("fixture", "local.ts", "old"))
    );
}
use agent_launcher::tui::{Intent, Scope};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ctrl(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}
#[test]
fn tui_search_and_provider_scope_filters_compose() {
    let mut local = session("one", 1, false);
    local.display.title = "Repository tools".into();
    let mut remote = session("two", 2, false);
    remote.stable_ref.host_identity = "remote.ts".into();
    remote.display.host = "remote.ts".into();
    remote.display.provider = "other".into();
    let mut picker = Picker::new(
        catalog(vec![local, remote]),
        vec![new_provider()],
        "local.ts".into(),
    );
    for ch in "rptls".chars() {
        picker.key(key(KeyCode::Char(ch)));
    }
    assert_eq!(picker.rows().len(), 1);
    picker.query.clear();
    picker.scope = Scope::AllHosts;
    picker.key(ctrl('p'));
    assert_eq!(picker.provider_filter.as_deref(), Some("fixture"));
    assert_eq!(picker.rows().len(), 2);
    picker.key(ctrl('p'));
    assert_eq!(picker.provider_filter.as_deref(), Some("other"));
    assert_eq!(picker.rows().len(), 1);
    picker.key(key(KeyCode::Tab));
    assert_eq!(picker.scope, Scope::ActiveOnly);
    assert!(picker.rows().is_empty());
}
#[test]
fn tui_refresh_preserves_stable_selection_or_reports_disappearance() {
    let chosen = session("chosen", 1, false);
    let reference = RowKey::Session(chosen.stable_ref.clone());
    let mut picker = Picker::new(catalog(vec![chosen.clone()]), vec![], "local.ts".into());
    picker.refresh(catalog(vec![session("newer", 5, false), chosen]), vec![]);
    assert_eq!(picker.selected, Some(reference));
    assert_eq!(picker.key(ctrl('r')), Intent::Refresh);
    picker.refresh(catalog(vec![session("newer", 5, false)]), vec![]);
    assert_eq!(picker.selected, None);
    assert!(picker.message.contains("disappeared"));
    assert_eq!(picker.key(key(KeyCode::Enter)), Intent::None);
}
fn live_session() -> SessionRecord {
    let mut row = session("live", 9, true);
    row.confidence = Confidence::ExactLiveBinding;
    row.live_binding = Some(LiveAgentBinding {
        agent: "fixture".into(),
        session_id: Some("live".into()),
        cwd: None,
        workspace_id: 1,
        workspace_name: "work".into(),
        tab_id: 2,
        tab_name: "named session".into(),
        pane_id: 3,
    });
    row.actions = vec![
        ActionPlan {
            attach: None,
            kind: ActionKind::Attach,
            ..new_provider().plan
        },
        ActionPlan {
            attach: None,
            kind: ActionKind::Resume,
            ..new_provider().plan
        },
    ];
    row
}
#[test]
fn tui_unsupported_capability_has_no_key_action() {
    let mut row = live_session();
    let reference = RowKey::Session(row.stable_ref.clone());
    let mut picker = Picker::new(
        catalog(vec![row.clone()]),
        vec![new_provider()],
        "local.ts".into(),
    );
    picker.key(key(KeyCode::Down));
    assert_eq!(
        picker.key(key(KeyCode::Enter)),
        Intent::Launch {
            key: reference.clone(),
            kind: ActionKind::Attach
        }
    );
    assert_eq!(picker.key(ctrl('f')), Intent::None);
    row.actions.push(ActionPlan {
        attach: None,
        kind: ActionKind::Fork,
        ..new_provider().plan
    });
    picker.refresh(catalog(vec![row]), vec![new_provider()]);
    assert_eq!(
        picker.key(ctrl('f')),
        Intent::Launch {
            key: reference,
            kind: ActionKind::Fork
        }
    );
    picker.key(ctrl('n'));
    assert_eq!(
        picker.key(key(KeyCode::Enter)),
        Intent::Launch {
            key: RowKey::New("fixture".into()),
            kind: ActionKind::New
        }
    );
    picker.key(key(KeyCode::Char('?')));
    assert!(picker.help);
    picker.key(key(KeyCode::Char('?')));
    assert!(!picker.help);
}
#[test]
fn tui_escape_exits_without_execution() {
    let mut picker = Picker::new(catalog(vec![]), vec![new_provider()], "local.ts".into());
    assert_eq!(picker.key(key(KeyCode::Esc)), Intent::Cancel);
    assert_eq!(picker.key(ctrl('c')), Intent::Cancel);
}
fn snapshot(picker: &Picker, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| picker.render(f)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn tui_hostile_display_fields_render_inertly() {
    let mut row = live_session();
    row.display.title = "\x1b[31mTitle\n\u{202e}".repeat(1000);
    row.actions[0].argv = vec!["DO_NOT_RENDER_TOOL_ARGUMENT".into()];
    row.warnings = vec!["\x1b]0;secret\x07 degraded\u{2066}".into()];
    let mut picker = Picker::new(catalog(vec![row]), vec![new_provider()], "local.ts".into());
    picker.key(key(KeyCode::Down));
    for (w, h) in [(110, 28), (32, 12), (1, 1), (0, 0)] {
        let rendered = snapshot(&picker, w, h);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{2066}'));
        assert!(!rendered.contains("DO_NOT_RENDER_TOOL_ARGUMENT"));
        assert!(!rendered.contains("secret"));
    }
    let rendered = snapshot(&picker, 110, 28);
    for label in [
        "New",
        "Active",
        "Last sent",
        "Reattach",
        "Confidence",
        "degraded",
    ] {
        assert!(rendered.contains(label), "missing {label}:\n{rendered}");
    }
    assert!(!rendered.contains("Ctrl+F"));
}
#[test]
fn tui_empty_and_failed_provider_snapshot() {
    let mut empty = catalog(vec![]);
    empty.providers.push(AgentSessionProviderStatus {
        name: "fixture".into(),
        ok: false,
        history_available: false,
        warning: None,
        error: Some("History unavailable".into()),
        session_count: 0,
    });
    let picker = Picker::new(empty, vec![], "local.ts".into());
    let rendered = snapshot(&picker, 90, 20);
    assert!(rendered.contains("No matching sessions"));
    assert!(rendered.contains("History unavailable"));
}
#[test]
fn tui_revalidation_rejects_disappearance_changed_plan_and_failed_provider() {
    let row = live_session();
    let key = RowKey::Session(row.stable_ref.clone());
    let picker = Picker::new(catalog(vec![row.clone()]), vec![], "local.ts".into());
    assert!(picker
        .revalidate(&key, ActionKind::Attach, catalog(vec![]), vec![])
        .is_err());
    let mut changed = row.clone();
    changed.actions[0].argv.push("changed".into());
    assert!(picker
        .revalidate(&key, ActionKind::Attach, catalog(vec![changed]), vec![])
        .is_err());
    let mut failed = catalog(vec![row.clone()]);
    failed.providers.push(AgentSessionProviderStatus {
        name: "fixture".into(),
        ok: false,
        history_available: true,
        warning: None,
        error: Some("failed".into()),
        session_count: 1,
    });
    assert!(picker
        .revalidate(&key, ActionKind::Attach, failed, vec![])
        .is_err());
    assert_eq!(
        picker
            .revalidate(&key, ActionKind::Attach, catalog(vec![row.clone()]), vec![])
            .unwrap(),
        row.actions[0]
    );
}
#[test]
fn tui_remote_resume_is_independent_of_failed_local_provider() {
    let mut row = session("remote-session", 1, false);
    row.stable_ref = StableRef::new("codex", "remote.ts", "remote-session");
    row.source = SessionSource::RemoteHistory;
    row.display.provider = "codex".into();
    row.display.host = "remote.ts".into();
    let mut plan = plan_resume("codex", "/tmp/project".into(), "remote-session");
    plan.transport = Transport::Ssh;
    plan.remote = Some(RemoteTarget {
        host_identity: "remote.ts".into(),
        ssh_target: "remote.ts".into(),
    });
    row.actions = vec![plan.clone()];
    let key = RowKey::Session(row.stable_ref.clone());
    let mut fresh = catalog(vec![row]);
    fresh.providers.push(AgentSessionProviderStatus {
        name: "codex".into(),
        ok: false,
        history_available: false,
        warning: None,
        error: Some("local history unavailable".into()),
        session_count: 0,
    });
    fresh.remote_hosts.push(RemoteHostStatus {
        host: "remote.ts".into(),
        ssh_target: "remote.ts".into(),
        ok: true,
        stale: false,
        error: None,
        session_count: 1,
        dropped_lines: 0,
        truncated_files: 0,
        warning: None,
        observed_at_unix_ms: Some(1),
    });
    let mut picker = Picker::new(fresh.clone(), vec![], "local.ts".into());
    picker.scope = Scope::AllHosts;
    assert_eq!(
        picker
            .revalidate(&key, ActionKind::Resume, fresh.clone(), vec![])
            .unwrap(),
        plan
    );
    fresh.remote_hosts[0].stale = true;
    assert!(picker
        .revalidate(&key, ActionKind::Resume, fresh, vec![])
        .is_err());
}
#[test]
fn tui_pty_cancel_exec_and_failed_exec_restore_terminal() {
    // Real PTY, synthetic HOME and only a fake executable. No operator history.
    let script = r#"
import os, pty, select, struct, subprocess, sys, termios, time, fcntl, json
binary, home = sys.argv[1:]
history = os.path.join(home, '.codex', 'sessions')
os.makedirs(history, exist_ok=True)
with open(os.path.join(history, 'fixture.jsonl'), 'w') as f:
    f.write(json.dumps({'type':'session_meta','payload':{'id':'fixture-session','cwd':home}})+'\n')
    f.write(json.dumps({'type':'event_msg','timestamp':'2026-09-06T15:00:00Z','payload':{'type':'user_message'}})+'\n')
for body, key, expected in [('#!/bin/sh\nexit 37\n', b'\x1b', 0),('#!/bin/sh\nexit 37\n', b'\r',37),('#!/missing-fixture-interpreter\n', b'\r',2)]:
    path=os.path.join(home,'codex')
    with open(path,'w') as f: f.write(body)
    os.chmod(path,0o755)
    master, slave=pty.openpty()
    fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',28,110,0,0))
    original=termios.tcgetattr(slave)
    proc=subprocess.Popen([binary],stdin=slave,stdout=slave,stderr=slave,cwd=home,env={'HOME':home,'PATH':home,'TERM':'xterm-256color','XDG_RUNTIME_DIR':home,'TZ':'UTC'})
    output=b''
    deadline=time.monotonic()+5
    try:
        while b'\x1b[?25l' not in output:
            assert time.monotonic()<deadline, 'picker did not render: '+repr(output)
            ready,_,_=select.select([master],[],[],0.1)
            if ready: output+=os.read(master,65536)
        assert b'09-06' in output and b'15:00' in output, 'message time not rendered'
        os.write(master,key)
        assert proc.wait(timeout=5)==expected
        assert termios.tcgetattr(slave)==original, 'terminal mode leaked'
        while select.select([master],[],[],0)[0]: output+=os.read(master,65536)
        assert b'\x1b[?1049l' in output, 'alternate screen leaked'
    finally:
        if proc.poll() is None: proc.kill();proc.wait()
        os.close(master);os.close(slave)
"#;
    let home = tempfile::TempDir::new().unwrap();
    let result = std::process::Command::new("python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_agent")])
        .arg(home.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
#[test]
fn tui_metadata_and_help_snapshots() {
    let mut row = live_session();
    row.actions.push(ActionPlan {
        attach: None,
        kind: ActionKind::Fork,
        ..new_provider().plan
    });
    let mut picker = Picker::new(
        catalog(vec![row, session("recent", 1, false)]),
        vec![new_provider()],
        "local.ts".into(),
    );
    picker.key(key(KeyCode::Down));
    for (name, help) in [("metadata", false), ("help", true)] {
        picker.help = help;
        let actual = snapshot(&picker, 110, 28);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/snapshots/{name}.txt"));
        if std::env::var_os("UPDATE_PICKER_SNAPSHOTS").is_some() {
            std::fs::write(&path, &actual).unwrap();
        }
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }
}
#[test]
fn tui_each_metadata_field_and_session_name_is_searchable() {
    let mut row = live_session();
    row.display.provider = "providertoken".into();
    row.display.title = "titletoken".into();
    row.display.cwd = "/directorytoken".into();
    row.display.repo_root = Some("/repotoken".into());
    row.display.host = "hosttoken".into();
    let mut picker = Picker::new(catalog(vec![row]), vec![], "local.ts".into());
    for query in [
        "providertoken",
        "titletoken",
        "directorytoken",
        "repotoken",
        "hosttoken",
        "named session",
    ] {
        picker.query = query.into();
        assert_eq!(picker.rows().len(), 1, "{query}");
    }
}
#[test]
fn tui_no_terminal_reports_scriptable_alternative() {
    let home = tempfile::TempDir::new().unwrap();
    for args in [vec![], vec!["--new"], vec!["--resume"]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent"))
            .args(args)
            .env_clear()
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires a terminal"));
        assert!(!output.stderr.contains(&0x1b));
    }
}

#[test]
fn tui_last_message_sort_crosses_active_groups_and_preserves_selection() {
    let mut active = live_session();
    active.last_user_message_at_unix_ms = Some(1000);
    let active_key = RowKey::Session(active.stable_ref.clone());
    let mut recent = session("newest-message", 1, false);
    recent.last_user_message_at_unix_ms = Some(2000);
    let recent_key = RowKey::Session(recent.stable_ref.clone());
    let unknown = session("unknown", u64::MAX, false);
    let mut picker = Picker::new(
        catalog(vec![active, unknown, recent]),
        vec![],
        "local.ts".into(),
    );
    picker.sort = agent_launcher::tui::Sort::LastMessage;
    assert_eq!(picker.rows()[0].key, recent_key);
    assert!(picker.rows()[2].label.starts_with("unknown"));
    picker.key(ctrl('s'));
    assert_eq!(picker.rows()[0].key, active_key);
    assert_eq!(picker.selected, Some(recent_key));
    picker.key(ctrl('s'));
    picker.key(ctrl('s'));
    let rendered = snapshot(&picker, 140, 28);
    assert!(rendered.contains("1970-01-01") || rendered.contains("1969-12-31"));
    assert!(rendered.contains("Last sent (newest first)"));
}

#[test]
fn tui_current_repository_includes_real_git_worktrees_and_excludes_other_roots() {
    use std::path::Path;
    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn row(id: &str, time: u64, cwd: &Path) -> SessionRecord {
        let raw = agent_session_core::legacy::parse_claude_session(
            Path::new("fixture.jsonl"),
            time,
            &[serde_json::json!({"sessionId":id,"cwd":cwd})],
        )
        .unwrap();
        let mut record = SessionRecord::from_legacy(&raw, "local.ts");
        record.last_user_message_at_unix_ms = Some(time);
        record
    }
    let root = tempfile::TempDir::new().unwrap();
    let repo = root.path().join("project");
    let linked = root.path().join("branch with spaces");
    let other = root.path().join("project-copy");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&other).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "fixture"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-qb",
            "feature",
            linked.to_str().unwrap(),
        ],
    );
    git(&other, &["init", "-q"]);
    let subdir = linked.join("nested");
    std::fs::create_dir(&subdir).unwrap();
    assert_eq!(repository_common_dir(&repo), repository_common_dir(&subdir));
    assert_ne!(repository_common_dir(&repo), repository_common_dir(&other));
    assert!(repository_common_dir(&root.path().join("missing")).is_none());
    let recent_other = row("other", 50, &other);
    let main = row("main", 20, &repo);
    let branch = row("branch", 30, &subdir);
    let branch_key = RowKey::Session(branch.stable_ref.clone());
    let other_key = RowKey::Session(recent_other.stable_ref.clone());
    let mut remote = row("remote", 60, &repo);
    remote.source = SessionSource::RemoteHistory;
    remote.stable_ref.host_identity = "remote.ts".into();
    let data = catalog(vec![recent_other, main, branch, remote]);
    let mut picker = Picker::new_in_directory(data.clone(), vec![], "local.ts".into(), &subdir);
    picker.scope = Scope::AllHosts;
    let rows = picker.rows();
    assert_eq!(rows[0].key, branch_key);
    assert!(rows[0].current_repository && rows[1].current_repository);
    assert!(rows[2..].iter().all(|r| !r.current_repository));
    assert!(snapshot(&picker, 140, 28).contains("This repository (all worktrees)"));
    assert_eq!(picker.selected, Some(branch_key.clone()));
    picker.refresh(data.clone(), vec![]);
    assert_eq!(picker.selected, Some(branch_key));
    picker.scope = Scope::Local;
    picker.key(ctrl('s'));
    assert_eq!(picker.rows()[0].key, other_key);
    let outside = Picker::new_in_directory(data, vec![], "local.ts".into(), root.path());
    assert_eq!(outside.rows()[0].key, other_key);
    assert!(snapshot(&outside, 140, 28).contains("Last sent (newest first)"));
    // A broken nested Git boundary must not silently inherit the outer repository.
    std::fs::write(subdir.join(".git"), "gitdir: /missing-fixture-git-dir\n").unwrap();
    assert!(repository_common_dir(&subdir).is_none());
}
