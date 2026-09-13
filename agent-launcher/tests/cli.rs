use std::process::Command;
use tempfile::TempDir;
fn command(home: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent"));
    cmd.env_clear()
        .env("HOME", home.path())
        .env("XDG_RUNTIME_DIR", home.path())
        .env("PATH", home.path())
        .current_dir(home.path());
    cmd
}
#[test]
fn json_catalog_works_without_taarof() {
    let home = TempDir::new().unwrap();
    let output = command(&home).arg("--json").output().unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema"], "agent.sessions.v2");
    assert_eq!(json["providers"].as_array().unwrap().len(), 6);
}
#[test]
fn doctor_reports_one_failed_provider_without_hiding_healthy_providers() {
    let home = TempDir::new().unwrap();
    let map = home.path().join(".pi/pi-acp/session-map.json");
    std::fs::create_dir_all(map.parent().unwrap()).unwrap();
    std::fs::write(map, "{").unwrap();
    let output = command(&home).arg("doctor").output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema"], "agent.doctor.v1");
    let providers = json["providers"].as_array().unwrap();
    assert!(providers
        .iter()
        .any(|p| p["id"] == "pi" && p["history"]["ok"] == false));
    assert!(providers
        .iter()
        .any(|p| p["id"] == "codex" && p["history"]["ok"] == true));
}
fn fake_provider(home: &TempDir, exit: u8) {
    use std::os::unix::fs::PermissionsExt;
    let path = home.path().join("codex");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" > launched\nexit {exit}\n"),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
fn history(home: &TempDir, provider: &str, id: &str) {
    let path = home.path().join(if provider == "codex" {
        ".codex/sessions/one.jsonl"
    } else {
        ".claude/projects/test/one.jsonl"
    });
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let value = if provider == "codex" {
        serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":home.path()}})
    } else {
        serde_json::json!({"sessionId":id,"cwd":home.path()})
    };
    std::fs::write(path, value.to_string()).unwrap();
}
#[test]
fn exact_resume_executes_structured_argv_in_recorded_cwd() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 0);
    let id = "literal 'quotes' ; $(false)";
    history(&home, "codex", id);
    let output = command(&home).arg("--json").output().unwrap();
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let selector = catalog["sessions"][0]["stable_ref"].to_string();
    let output = command(&home)
        .args(["--resume", &selector])
        .current_dir("/")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        std::fs::read_to_string(home.path().join("launched")).unwrap(),
        format!("{}\nresume\n{id}\n", home.path().display())
    );
}
#[test]
fn malformed_and_option_shaped_selections_fail_without_launch() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 0);
    history(&home, "codex", "--help");
    for selector in ["--help", "{broken", "missing", ""] {
        assert!(!command(&home)
            .args(["--resume", selector])
            .status()
            .unwrap()
            .success());
    }
    assert!(!home.path().join("launched").exists());
}
#[test]
fn ambiguous_selector_fails_without_launch() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 0);
    history(&home, "codex", "same");
    history(&home, "claude", "same");
    let output = command(&home).args(["--resume", "same"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous"));
    assert!(!home.path().join("launched").exists());
}
#[test]
fn provider_exit_status_is_propagated() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 37);
    assert_eq!(
        command(&home)
            .args(["--new", "codex"])
            .status()
            .unwrap()
            .code(),
        Some(37)
    );
}
#[test]
fn omitted_targets_and_unknown_providers_fail_closed() {
    let home = TempDir::new().unwrap();
    for args in [
        vec!["--new"],
        vec!["--resume"],
        vec!["--new", "unknown"],
        vec!["--new", "codex"],
    ] {
        assert!(!command(&home).args(args).output().unwrap().status.success());
    }
}
#[test]
fn stale_cwd_fails_without_launch() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 0);
    history(&home, "codex", "one");
    let file = home.path().join(".codex/sessions/one.jsonl");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    value["payload"]["cwd"] = serde_json::json!(home.path().join("gone"));
    std::fs::write(file, value.to_string()).unwrap();
    assert!(!command(&home)
        .args(["--resume", "one"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!home.path().join("launched").exists());
}
#[test]
fn installed_bundle_contains_agent_binary() {
    let home = TempDir::new().unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let fixture = home.path().join("repo");
    for directory in [
        "packaging",
        "taarof-app/resources",
        "examples",
        "taarof-cli",
    ] {
        let target = fixture.join(directory);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        assert!(Command::new("cp")
            .args(["-a"])
            .arg(repo.join(directory))
            .arg(target)
            .status()
            .unwrap()
            .success());
    }
    std::fs::create_dir_all(fixture.join("taarof-app/target/release")).unwrap();
    std::fs::copy(
        repo.join("taarof-app/Cargo.toml"),
        fixture.join("taarof-app/Cargo.toml"),
    )
    .unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_agent"),
        fixture.join("taarof-app/target/release/taarof-app"),
    )
    .unwrap();
    std::fs::create_dir_all(fixture.join("taarof-web/dist")).unwrap();
    std::fs::write(fixture.join("taarof-web/dist/index.html"), "fixture").unwrap();
    let prefix = home.path().join("install");
    let output = Command::new("bash")
        .arg(fixture.join("packaging/linux/install-local.sh"))
        .arg(&prefix)
        .env_remove("CARGO_TARGET_DIR")
        .env("TAAROF_INSTALL_AGENT_BINARY", env!("CARGO_BIN_EXE_agent"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Cargo's shared target directory must win over stale crate-local outputs.
    let shared = fixture.join("shared-target");
    std::fs::create_dir_all(shared.join("release")).unwrap();
    for name in ["agent", "taarof-app"] {
        std::fs::copy(
            env!("CARGO_BIN_EXE_agent"),
            shared.join("release").join(name),
        )
        .unwrap();
    }
    let shared_prefix = home.path().join("shared-install");
    let result = Command::new("bash")
        .arg(fixture.join("packaging/linux/install-local.sh"))
        .arg(&shared_prefix)
        .env("CARGO_TARGET_DIR", &shared)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read(shared_prefix.join("bin/agent")).unwrap(),
        std::fs::read(env!("CARGO_BIN_EXE_agent")).unwrap()
    );
    for arg in ["--version", "--help", "providers"] {
        assert!(Command::new(prefix.join("bin/agent"))
            .arg(arg)
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_RUNTIME_DIR", home.path())
            .output()
            .unwrap()
            .status
            .success());
    }
}

#[test]
fn missing_or_relative_home_fails_without_guessing_history() {
    let home = TempDir::new().unwrap();
    for argument in ["--json", "doctor", "providers"] {
        let missing = command(&home)
            .env_remove("HOME")
            .arg(argument)
            .output()
            .unwrap();
        assert!(!missing.status.success());
        let relative = command(&home)
            .env("HOME", ".")
            .arg(argument)
            .output()
            .unwrap();
        assert!(!relative.status.success());
    }
}

#[test]
fn corrupt_history_preserves_records_but_refuses_execution_until_refresh() {
    let home = TempDir::new().unwrap();
    fake_provider(&home, 0);
    history(&home, "codex", "valid");
    let broken = home.path().join(".codex/sessions/broken.jsonl");
    std::fs::write(&broken, "{partial").unwrap();
    let output = command(&home).arg("--json").output().unwrap();
    assert!(output.status.success());
    let catalog: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(catalog["sessions"].as_array().unwrap().len(), 1);
    assert!(!command(&home)
        .args(["--resume", "valid"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!home.path().join("launched").exists());
    std::fs::write(
        &broken,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"second\",\"cwd\":\"/\"}}\n",
    )
    .unwrap();
    assert!(command(&home)
        .args(["--resume", "valid"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn catalog_and_doctor_exclude_prompt_and_environment_values() {
    let home = TempDir::new().unwrap();
    history(&home, "codex", "valid");
    let path = home.path().join(".codex/sessions/one.jsonl");
    let mut source = std::fs::read_to_string(&path).unwrap();
    source.push_str("\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"text\":\"SYNTHETIC_PRIVATE_PROMPT\"}]}}\n");
    std::fs::write(&path, source).unwrap();
    for argument in ["--json", "providers", "doctor"] {
        let result = command(&home)
            .env("SYNTHETIC_PRIVATE_ENV", "SYNTHETIC_VALUE_SENTINEL")
            .arg(argument)
            .output()
            .unwrap();
        assert!(result.status.success());
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(!output.contains("SYNTHETIC_PRIVATE_PROMPT"));
        assert!(!output.contains("SYNTHETIC_VALUE_SENTINEL"));
    }
}

#[test]
fn build_info_is_embedded_and_independent_of_runtime_environment() {
    let home = TempDir::new().unwrap();
    let output = command(&home)
        .arg("--build-info")
        .env_remove("HOME")
        .env("TAAROF_SOURCE_REVISION", "runtime-must-not-override-build")
        .output()
        .unwrap();
    assert!(output.status.success());
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["schema"], "agent.build.v1");
    assert_eq!(info, agent_launcher::build_info());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("runtime-must-not-override-build"));
}
