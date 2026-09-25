use agent_launcher::enrichment::{merge_catalog, ssh_command, storage_key, SocketClient};
use agent_session_core::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use tempfile::TempDir;

fn catalog() -> SessionCatalog {
    SessionCatalog::from_discovery(AgentSessionDiscovery::default(), "local.ts")
}
fn remote_catalog(stale: bool) -> SessionCatalog {
    SessionCatalog::from_discovery(
        AgentSessionDiscovery {
            sessions: vec![AgentSessionRecord {
                agent: "codex".into(),
                session_id: "id with 'quotes' ; $(false)".into(),
                title: "private prompt".into(),
                cwd: "/tmp/it's a project".into(),
                host: Some("label".into()),
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: 5,
                last_user_message_at_unix_ms: None,
                status: "recent".into(),
                live_binding: None,
                resume_command: Some("DO NOT EXECUTE".into()),
                resume_unavailable_reason: None,
            }],
            remote_hosts: vec![RemoteHostStatus {
                host: "label".into(),
                ssh_target: "user@remote.ts".into(),
                ok: !stale,
                stale,
                error: stale.then(|| "unreachable".into()),
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
}
#[test]
fn taarof_enrichment_stopped_taarof_falls_back_to_local_catalog() {
    let dir = TempDir::new().unwrap();
    assert!(SocketClient::new(dir.path().join("absent.sock")).is_err());
    assert!(catalog().sessions.is_empty());
}
#[test]
fn taarof_enrichment_stale_remote_host_remains_visible_and_nonblocking() {
    let mut local = catalog();
    merge_catalog(&mut local, remote_catalog(true)).unwrap();
    assert_eq!(local.sessions.len(), 1);
    assert!(local.sessions[0].actions.is_empty());
    assert!(local.remote_hosts[0].stale);
    assert!(!local.sessions[0].warnings.is_empty());
}
#[test]
fn taarof_enrichment_remote_plan_preserves_argv_and_tty_target() {
    let catalog = remote_catalog(false);
    let plan = &catalog.sessions[0].actions[0];
    let cmd = ssh_command(plan).unwrap();
    let args: Vec<_> = cmd
        .get_args()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    assert!(args.contains(&"-tt".into()));
    assert!(args.contains(&"user@remote.ts".into()));
    // Execute the generated SSH remote command in an isolated fake provider shell.
    let home = TempDir::new().unwrap();
    let fake = home.path().join("codex");
    std::fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut local_plan = plan.clone();
    local_plan.cwd = home.path().to_owned();
    let command = ssh_command(&local_plan).unwrap();
    let script = command.get_args().last().unwrap();
    let result = std::process::Command::new("/bin/sh")
        .args(["-c"])
        .arg(script)
        .env_clear()
        .env("PATH", home.path())
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "{}\nresume\n{}\n",
            home.path().display(),
            catalog.sessions[0].stable_ref.session_id
        )
    );
    let mut hostile = plan.clone();
    hostile.remote.as_mut().unwrap().ssh_target = "-oProxyCommand=evil".into();
    assert!(ssh_command(&hostile).is_err());
}
#[test]
fn taarof_enrichment_eof_framed_socket_uses_v2_and_rejects_public_runtime_dir() {
    let dir = TempDir::new().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("app.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Production socket::read_socket_request reads through EOF, not newline.
        // Bound the fixture too so a client framing regression cannot hang tests.
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        Read::by_ref(&mut stream)
            .take(65537)
            .read_to_end(&mut bytes)
            .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(request["schema"], "agent.sessions.v2");
        writeln!(
            stream,
            "{}",
            serde_json::json!({"ok":true,"data":remote_catalog(false)})
        )
        .unwrap();
    });
    let client = SocketClient::new(path).unwrap();
    let mut local = catalog();
    client.enrich(&mut local).unwrap();
    server.join().unwrap();
    assert_eq!(local.sessions.len(), 1);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(SocketClient::new(dir.path().join("app.sock")).is_err());
}
#[test]
fn taarof_enrichment_named_storage_key_matches_cli_contract() {
    assert_eq!(storage_key(""), "default");
    assert_ne!(storage_key("one/two"), storage_key("one?two"));
    assert_eq!(storage_key(" one/two "), storage_key("one/two"));
    let name = "界";
    let digest = storage_key(name);
    assert!(digest.starts_with("e7958c-"));
}

fn private_temp() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
fn cli(home: &TempDir) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_agent"));
    cmd.env_clear()
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("PATH", home.path());
    cmd
}
#[test]
fn named_missing_runtime_preserves_local_history_with_explicit_diagnostic() {
    let dir = private_temp();
    let history = dir.path().join(".codex/sessions/test.jsonl");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(
        history,
        serde_json::json!({"type":"session_meta","payload":{"id":"retained","cwd":dir.path()}})
            .to_string(),
    )
    .unwrap();
    let out = cli(&dir)
        .args(["--session", "missing/runtime", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let catalog: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        catalog["sessions"][0]["stable_ref"]["session_id"],
        "retained"
    );
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("Named Taarof runtime is unavailable"));
}
#[test]
fn named_runtime_uses_hashed_registry_and_attach_sends_full_expected_identity() {
    let dir = private_temp();
    let path = dir.path().join("named.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        dir.path()
            .join(format!("taarof-current-{}.json", storage_key("one/two"))),
        serde_json::json!({"socket_path":path}).to_string(),
    )
    .unwrap();
    let evidence = LiveSessionEvidence {
        stable_ref: StableRef::new("codex", "remote.ts", "exact"),
        cwd: "/project".into(),
        tmux_session: Some("duplicate-name".into()),
        ssh_target: Some("remote.ts".into()),
        binding: LiveAgentBinding {
            agent: "codex".into(),
            session_id: Some("exact".into()),
            cwd: Some("/project".into()),
            workspace_id: 1,
            workspace_name: "ws".into(),
            tab_id: 2,
            tab_name: "tab".into(),
            pane_id: 3,
        },
    };
    let target = evidence.attach_target().unwrap();
    let selector = serde_json::to_string(&target.stable_ref).unwrap();
    let mut incoming = catalog();
    incoming.sessions.push(SessionRecord::from_legacy(
        &AgentSessionRecord {
            agent: "codex".into(),
            session_id: "exact".into(),
            title: String::new(),
            cwd: "/project".into(),
            host: Some("remote.ts".into()),
            repo_root: None,
            started_at_unix_ms: None,
            updated_at_unix_ms: 1,
            last_user_message_at_unix_ms: None,
            status: "recent".into(),
            live_binding: None,
            resume_command: None,
            resume_unavailable_reason: None,
        },
        "local.ts",
    ));
    enrich_live(&mut incoming, &[evidence]);
    let server = std::thread::spawn(move || {
        for attach in [false, true] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if attach {
                assert_eq!(request["action"], "attach-session");
                assert_eq!(
                    request["expected_agent"],
                    serde_json::to_value(&target).unwrap()
                );
                assert_eq!(request["ssh_target"], "remote.ts");
                writeln!(stream, "{{\"ok\":true}}").unwrap();
            } else {
                assert_eq!(request["schema"], "agent.sessions.v2");
                writeln!(stream, "{}", serde_json::json!({"ok":true,"data":incoming})).unwrap();
            }
        }
    });
    let output = cli(&dir)
        .args(["--session", "one/two", "--attach", &selector])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
}
#[test]
fn malformed_catalog_does_not_partially_merge_and_metadata_only_row_never_attaches() {
    let mut incoming = remote_catalog(false);
    let row = incoming.sessions[0].clone();
    incoming.sessions.push(row);
    let mut local = catalog();
    assert!(merge_catalog(&mut local, incoming).is_err());
    assert!(local.sessions.is_empty());
    let mut incoming = remote_catalog(false);
    incoming.sessions[0].confidence = Confidence::ProviderIdentity;
    incoming.sessions[0].state = SessionState::Active;
    merge_catalog(&mut local, incoming).unwrap();
    assert_eq!(local.sessions[0].state, SessionState::Recent);
    assert!(local.sessions[0]
        .actions
        .iter()
        .all(|a| a.kind != ActionKind::Attach));
}

#[test]
fn fifo_runtime_registry_cannot_block_local_discovery() {
    use std::os::unix::ffi::OsStrExt;
    let dir = private_temp();
    let path = std::ffi::CString::new(
        dir.path()
            .join("taarof-current.json")
            .as_os_str()
            .as_bytes(),
    )
    .unwrap();
    // Valid NUL-terminated pathname; no existing file is overwritten.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let start = std::time::Instant::now();
    let output = cli(&dir).arg("--json").output().unwrap();
    assert!(output.status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("bounded regular file"));
}

#[test]
fn remote_resume_executes_fake_ssh_with_tty_and_propagates_exit_status() {
    let dir = private_temp();
    let path = dir.path().join("ssh.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        dir.path().join("ssh"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > ssh-argv\nexit 37\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.path().join("ssh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let incoming = remote_catalog(false);
    let selector = serde_json::to_string(&incoming.sessions[0].stable_ref).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        writeln!(stream, "{}", serde_json::json!({"ok":true,"data":incoming})).unwrap();
    });
    let output = cli(&dir)
        .env("TAAROF_SOCK", path)
        .current_dir(dir.path())
        .args(["--resume", &selector])
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(37));
    let args = std::fs::read_to_string(dir.path().join("ssh-argv")).unwrap();
    assert!(args.starts_with("-tt\n-oBatchMode=yes\n-oConnectTimeout=3\n--\nuser@remote.ts\n"));
    assert!(args.contains("cd -- '/tmp/it'\\''s a project' && exec 'codex' 'resume'"));
}
#[test]
fn stalled_socket_response_has_one_bounded_deadline() {
    let dir = private_temp();
    let path = dir.path().join("stalled.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3000));
    });
    let started = std::time::Instant::now();
    let error = SocketClient::new(path)
        .unwrap()
        .enrich(&mut catalog())
        .unwrap_err();
    assert!(error.contains("timed out"));
    assert!(started.elapsed() < std::time::Duration::from_millis(2800));
    server.join().unwrap();
}

#[test]
fn picker_prefers_guarded_attach_and_restores_terminal_with_named_runtime() {
    let dir = private_temp();
    let hostname = agent_launcher::local_host().unwrap();
    let history = dir.path().join(".codex/sessions/one.jsonl");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(
        history,
        serde_json::json!({"type":"session_meta","payload":{"id":"exact","cwd":dir.path()}})
            .to_string(),
    )
    .unwrap();
    let path = dir.path().join("picker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        dir.path().join(format!(
            "taarof-current-{}.json",
            storage_key("picker/runtime")
        )),
        serde_json::json!({"socket_path":path}).to_string(),
    )
    .unwrap();
    let mut incoming = SessionCatalog::from_discovery(
        AgentSessionDiscovery {
            sessions: vec![AgentSessionRecord {
                agent: "codex".into(),
                session_id: "exact".into(),
                title: String::new(),
                cwd: dir.path().to_string_lossy().into_owned(),
                host: None,
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: 1,
                last_user_message_at_unix_ms: None,
                status: "recent".into(),
                live_binding: None,
                resume_command: None,
                resume_unavailable_reason: None,
            }],
            ..Default::default()
        },
        &hostname,
    );
    let evidence = LiveSessionEvidence {
        stable_ref: StableRef::new("codex", &hostname, "exact"),
        cwd: dir.path().to_owned(),
        tmux_session: Some("picker-tmux".into()),
        ssh_target: None,
        binding: LiveAgentBinding {
            agent: "codex".into(),
            session_id: Some("exact".into()),
            cwd: None,
            workspace_id: 1,
            workspace_name: "synthetic".into(),
            tab_id: 2,
            tab_name: "synthetic".into(),
            pane_id: 3,
        },
    };
    let target = evidence.attach_target().unwrap();
    enrich_live(&mut incoming, &[evidence]);
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for index in 0..3 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "picker did not request expected socket action"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream).read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if index < 2 {
                assert_eq!(request["schema"], "agent.sessions.v2");
                writeln!(stream, "{}", serde_json::json!({"ok":true,"data":incoming})).unwrap();
            } else {
                assert_eq!(request["action"], "attach-session");
                assert_eq!(
                    request["expected_agent"],
                    serde_json::to_value(&target).unwrap()
                );
                writeln!(stream, "{{\"ok\":true}}").unwrap();
            }
        }
    });
    let script = r#"
import os,pty,select,subprocess,sys,termios,time,fcntl,struct
binary,home=sys.argv[1:]
master,slave=pty.openpty()
fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',28,110,0,0))
old=termios.tcgetattr(slave)
proc=subprocess.Popen([binary,'--session','picker/runtime'],stdin=slave,stdout=slave,stderr=slave,cwd=home,env={'HOME':home,'XDG_RUNTIME_DIR':home,'PATH':home,'TERM':'xterm-256color'})
try:
    output=b'';deadline=time.monotonic()+7
    while b'Reattach' not in output and proc.poll() is None:
        assert time.monotonic()<deadline
        if select.select([master],[],[],.05)[0]:output+=os.read(master,65536)
    assert b'Reattach' in output
    os.write(master,b'\r')
    assert proc.wait(timeout=7)==0
    assert termios.tcgetattr(slave)==old
finally:
    if proc.poll() is None:proc.kill();proc.wait()
    os.close(master);os.close(slave)
"#;
    let result = std::process::Command::new("python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_agent")])
        .arg(dir.path())
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
