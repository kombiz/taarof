use agent_session_core::adapters::ExternalRegistry;
#[test]
fn external_adapter_path_discovery_without_manifest_does_not_enable_adapter() {
    let temp = tempfile::tempdir().unwrap();
    let registry = ExternalRegistry::load(&temp.path().join("absent"));
    assert!(registry.adapters().is_empty());
    assert!(registry.diagnostics().is_empty());
}
#[test]
fn external_adapter_unsupported_manifest_and_builtin_override_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    for (file, body) in [("bad", "schema = 2\nid = 'synthetic'"), ("builtin", "schema = 1\nid = 'codex'\ndisplay_name = 'Fake'\ncommand = ['/bin/false']\nenabled = true\ncapabilities = ['new', 'resume']")] {
        std::fs::write(dir.path().join(format!("{file}.toml")), body).unwrap();
    }
    let registry = ExternalRegistry::load(dir.path());
    assert!(registry.adapters().is_empty());
    assert_eq!(registry.diagnostics().len(), 2);
    assert!(registry.diagnostics().iter().all(|p| !p.ok));
}
fn synthetic(dir: &std::path::Path, id: &str, command: &[&str]) {
    std::fs::write(dir.join(format!("{id}.toml")), format!("schema = 1\nid = {id:?}\ndisplay_name = 'Synthetic'\ncommand = {}\nenabled = true\ncapabilities = ['new', 'resume']\n", serde_json::to_string(command).unwrap())).unwrap();
}
#[test]
fn external_adapter_synthetic_new_resume_and_hostile_display() {
    use agent_session_core::{ProviderAdapter, SessionCatalog};
    let dir = tempfile::tempdir().unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/agent-provider-adapter/adapter.py");
    synthetic(
        dir.path(),
        "synthetic",
        &["/usr/bin/python3", script.to_str().unwrap()],
    );
    let registry = ExternalRegistry::load(dir.path());
    let adapter = &registry.adapters()[0];
    assert!(adapter.probe().ok);
    assert!(adapter.plan_new(dir.path().into()).is_some());
    let mut catalog = SessionCatalog::from_discovery(Default::default(), "fixture.ts");
    registry.merge_catalog(&mut catalog, "fixture.ts");
    assert_eq!(catalog.sessions.len(), 1);
    assert!(!catalog.sessions[0].display.title.contains(char::is_control));
    assert_eq!(catalog.sessions[0].actions.len(), 1);
    assert!(adapter.check_conformance(dir.path()).is_ok());
}
#[test]
fn external_adapter_failures_are_bounded_and_isolated() {
    use agent_session_core::ProviderAdapter;
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("timeout", "import time; time.sleep(10)", "timed out"),
        ("oversize", "print('x' * 300000)", "limit"),
        ("stderr", "import sys; sys.stderr.write('x' * 9000)", "limit"),
        ("malformed", "print('bad json')", "malformed"),
        ("crash", "raise SystemExit(2)", "failed"),
        ("protocol", "print('{\"protocol\":2,\"id\":\"protocol\",\"display_name\":\"Synthetic\",\"capabilities\":[\"new\",\"resume\"]}')", "mismatch"),
        ("descendant", "import os,time; p=os.fork(); time.sleep(10) if p == 0 else None", "timed out"),
    ];
    for (id, code, _) in cases {
        synthetic(dir.path(), id, &["/usr/bin/python3", "-c", code]);
    }
    synthetic(dir.path(), "missing", &["/missing-adapter"]);
    let registry = ExternalRegistry::load(dir.path());
    let start = std::time::Instant::now();
    for adapter in registry.adapters() {
        let status = adapter.probe();
        assert!(!status.ok, "{}", adapter.manifest.id);
        if let Some((_, _, expected)) = cases.iter().find(|(id, _, _)| *id == adapter.manifest.id) {
            assert!(
                status.error.unwrap().contains(expected),
                "{}",
                adapter.manifest.id
            );
        }
    }
    assert!(start.elapsed().as_secs() < 7);
}
#[test]
fn external_adapter_receives_minimal_environment() {
    use agent_session_core::ProviderAdapter;
    let dir = tempfile::tempdir().unwrap();
    let code = "import os,json,sys; r=json.loads(sys.stdin.readline()); assert not sys.stdin.isatty(); assert sys.stdin.read()==''; assert set(os.environ)<=set(['PATH','LANG','LC_CTYPE','HOME','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_STATE_HOME']); print(json.dumps({'protocol':1,'id':'env','display_name':'Synthetic','capabilities':['new','resume']} if r['operation']=='metadata' else {'protocol':1,'available':True} if r['operation']=='probe' else {'protocol':1,'sessions':[]}))";
    synthetic(dir.path(), "env", &["/usr/bin/python3", "-c", code]);
    assert!(ExternalRegistry::load(dir.path()).adapters()[0].probe().ok);
}
#[test]
fn external_adapter_cli_catalog_new_resume_doctor_and_conformance() {
    let home = tempfile::tempdir().unwrap();
    let manifests = home.path().join(".config/agent/providers.d");
    std::fs::create_dir_all(&manifests).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/agent-provider-adapter/adapter.py");
    synthetic(
        &manifests,
        "synthetic",
        &["/usr/bin/python3", script.to_str().unwrap()],
    );
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_agent"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", &runtime)
            .args(args)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env("SHOULD_NOT_REACH_ADAPTER", "fixture-only")
            .output()
            .unwrap()
    };
    let output = run(&["--json"]);
    assert!(output.status.success());
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        catalog["sessions"][0]["stable_ref"]["provider_id"],
        "synthetic"
    );
    for args in [
        vec!["--new", "synthetic"],
        vec!["--resume", "fixture with 'quotes'"],
        vec!["conformance", "synthetic"],
    ] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    synthetic(&manifests, "missing", &["/missing-provider"]);
    let output = run(&["doctor"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "synthetic" && p["history"]["ok"] == true));
    assert!(report["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "missing" && p["history"]["ok"] == false));
}
#[test]
fn external_adapter_duplicate_unknown_disabled_and_invalid_plan_fail_closed() {
    use agent_session_core::{ProviderAdapter, SessionCatalog};
    let dir = tempfile::tempdir().unwrap();
    synthetic(dir.path(), "duplicate", &["/bin/false"]);
    std::fs::copy(
        dir.path().join("duplicate.toml"),
        dir.path().join("second.toml"),
    )
    .unwrap();
    synthetic(dir.path(), "disabled", &["/bin/false"]);
    let file = dir.path().join("disabled.toml");
    std::fs::write(
        &file,
        std::fs::read_to_string(&file)
            .unwrap()
            .replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    synthetic(dir.path(), "unknown", &["/bin/false"]);
    let file = dir.path().join("unknown.toml");
    std::fs::write(
        &file,
        format!(
            "{}\nsecret = 'forbidden'",
            std::fs::read_to_string(&file).unwrap()
        ),
    )
    .unwrap();
    let registry = ExternalRegistry::load(dir.path());
    assert!(registry.adapters().is_empty());
    assert_eq!(registry.diagnostics().len(), 2);
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/agent-provider-adapter/adapter.py"),
    )
    .unwrap();
    let bad = dir.path().join("bad.py");
    std::fs::write(
        &bad,
        script.replace("cwd=request[\"cwd\"]", "cwd=\"relative\""),
    )
    .unwrap();
    synthetic(
        dir.path(),
        "synthetic",
        &["/usr/bin/python3", bad.to_str().unwrap()],
    );
    let registry = ExternalRegistry::load(dir.path());
    assert!(registry.adapters()[0].plan_new(dir.path().into()).is_none());
    let mut catalog = SessionCatalog::from_discovery(Default::default(), "fixture.ts");
    registry.merge_catalog(&mut catalog, "fixture.ts");
    assert!(catalog.sessions.is_empty());
    assert!(catalog
        .providers
        .iter()
        .any(|p| p.name == "synthetic" && !p.ok));
}
#[test]
fn external_adapter_duplicate_sessions_and_capability_lies_fail_closed() {
    use agent_session_core::ProviderAdapter;
    let dir = tempfile::tempdir().unwrap();
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/agent-provider-adapter/adapter.py"),
    )
    .unwrap();
    let bad = dir.path().join("bad.py");
    std::fs::write(
        &bad,
        script.replace(
            "print(json.dumps(result))",
            "if 'sessions' in result: result['sessions'] *= 2\nprint(json.dumps(result))",
        ),
    )
    .unwrap();
    synthetic(
        dir.path(),
        "synthetic",
        &["/usr/bin/python3", bad.to_str().unwrap()],
    );
    assert!(!ExternalRegistry::load(dir.path()).adapters()[0].probe().ok);
    std::fs::write(
        &bad,
        script.replace(
            "capabilities=[\"new\", \"resume\"]",
            "capabilities=[\"new\"]",
        ),
    )
    .unwrap();
    assert!(!ExternalRegistry::load(dir.path()).adapters()[0].probe().ok);
}
#[test]
fn external_adapter_builtin_alias_cannot_gain_builtin_identity() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["claude-code", "codex-cli", "copilot-cli", "pii"] {
        synthetic(dir.path(), id, &["/bin/false"]);
    }
    assert!(ExternalRegistry::load(dir.path()).adapters().is_empty());
}
#[test]
fn external_adapter_missing_planned_executable_is_diagnosed() {
    use agent_session_core::SessionCatalog;
    let dir = tempfile::tempdir().unwrap();
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/agent-provider-adapter/adapter.py"),
    )
    .unwrap();
    let bad = dir.path().join("bad.py");
    std::fs::write(
        &bad,
        script.replace("/usr/bin/printf", "/nonexistent-fixture-launch-program"),
    )
    .unwrap();
    synthetic(
        dir.path(),
        "synthetic",
        &["/usr/bin/python3", bad.to_str().unwrap()],
    );
    let registry = ExternalRegistry::load(dir.path());
    assert!(registry.adapters()[0]
        .check_conformance(dir.path())
        .is_err());
    let mut catalog = SessionCatalog::from_discovery(Default::default(), "fixture.ts");
    registry.merge_catalog(&mut catalog, "fixture.ts");
    assert!(catalog.sessions.is_empty());
    assert!(!catalog.providers[0].ok);
    assert!(catalog.providers[0]
        .error
        .as_deref()
        .unwrap()
        .contains("executable"));
}
#[test]
fn external_adapter_unavailable_probe_cannot_launch_new() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".config/agent/providers.d");
    std::fs::create_dir_all(&dir).unwrap();
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/agent-provider-adapter/adapter.py"),
    )
    .unwrap();
    let bad = home.path().join("bad.py");
    std::fs::write(&bad, script.replace("available=True", "available=False")).unwrap();
    synthetic(
        &dir,
        "synthetic",
        &["/usr/bin/python3", bad.to_str().unwrap()],
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agent"))
        .env_clear()
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["--new", "synthetic"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
#[test]
fn external_adapter_picker_new_requires_available_manifest_and_plan() {
    let script = r#"
import os, pty, select, struct, subprocess, sys, termios, time, fcntl
binary, home, example = sys.argv[1:]
manifest_dir=os.path.join(home,'.config','agent','providers.d')
os.makedirs(manifest_dir)
source=open(example).read()
for mode in ("valid", "unavailable", "missing-plan", "missing-adapter", "disabled"):
    unavailable=mode != "valid"
    adapter=os.path.join(home,'adapter.py')
    code=source.replace('available=True','available=False') if mode == 'unavailable' else source
    if mode == 'missing-plan': code=code.replace('/usr/bin/printf','/nonexistent-fixture-program')
    with open(adapter,'w') as f: f.write(code)
    with open(os.path.join(manifest_dir,'synthetic.toml'),'w') as f:
        import json
        enabled='false' if mode == 'disabled' else 'true'
        command=['/nonexistent-fixture-adapter'] if mode == 'missing-adapter' else ['/usr/bin/python3',adapter]
        f.write('schema = 1\nid = "synthetic"\ndisplay_name = "Synthetic"\nenabled = '+enabled+'\ncapabilities = ["new", "resume"]\ncommand = '+json.dumps(command)+'\n')
    master,slave=pty.openpty()
    fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',28,110,0,0))
    proc=subprocess.Popen([binary,'--new'],stdin=slave,stdout=slave,stderr=slave,cwd=home,env={'HOME':home,'PATH':'/usr/bin:/bin','TERM':'xterm-256color','XDG_RUNTIME_DIR':home})
    output=b''
    try:
        deadline=time.monotonic()+8
        while b'\x1b[?25l' not in output:
            assert time.monotonic()<deadline, 'picker did not render'
            if select.select([master],[],[],0.1)[0]: output+=os.read(master,65536)
        import re
        visible=b' '.join(re.sub(rb'\x1b\[[0-9;?]*[A-Za-z]',b' ',output).split())
        assert (b'New synthetic' in visible) == (not unavailable), repr(visible)
        os.write(master,b'\x1b' if unavailable else b'synthetic\r')
        assert proc.wait(timeout=8)==0
        while select.select([master],[],[],0)[0]: output+=os.read(master,65536)
        assert (b'Synthetic plan-new' in output) == (not unavailable)
    finally:
        if proc.poll() is None: proc.kill();proc.wait()
        os.close(master);os.close(slave)
"#;
    let home = tempfile::tempdir().unwrap();
    let result = std::process::Command::new("/usr/bin/python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_agent")])
        .arg(home.path())
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../examples/agent-provider-adapter/adapter.py"),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
