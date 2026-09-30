use super::*;
use std::os::unix::fs::PermissionsExt;

struct Fixture(PathBuf);

impl Fixture {
    fn new(script: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "taarof-mise-bounded-{}-{}",
            std::process::id(),
            next_discovery_generation()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("mise.toml"), "[tools]\n").unwrap();
        let fixture = Self(root);
        fixture.script(script);
        fixture
    }

    fn script(&self, script: &str) {
        for name in ["mise", "ssh"] {
            let path = self.0.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    fn local(&self) -> DiscoveryTarget {
        DiscoveryTarget::Local {
            cwd: self.0.to_string_lossy().into_owned(),
            binary_path: Some(self.0.join("mise")),
        }
    }

    fn remote(&self) -> DiscoveryTarget {
        DiscoveryTarget::Remote {
            host: "fixture.ts".into(),
            cwd: "/fixture".into(),
            ssh_argv: vec![
                self.0.join("ssh").to_string_lossy().into_owned(),
                "fixture.ts".into(),
            ],
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn reset() {
    clear_task_discovery_cache_for_test();
    clear_tool_version_cache_for_test();
    clear_discover_tasks_test_probe();
    clear_tool_version_test_probe();
}

#[test]
fn all_four_commands_distinguish_empty_invalid_and_failed_output() {
    let _guard = task_discovery_test_guard();
    reset();
    let fixture = Fixture::new("printf '[]'");
    for target in [fixture.local(), fixture.remote()] {
        assert_eq!(try_discover_tasks_for_target(&target), Ok(Vec::new()));
        assert_eq!(chip::discover_tool_version_chip(&target), Ok(None));
    }
    fixture.script("printf '{broken'");
    for target in [fixture.local(), fixture.remote()] {
        assert_eq!(
            try_discover_tasks_for_target(&target),
            Err(DiscoveryFailure::InvalidJson)
        );
        assert_eq!(
            chip::discover_tool_version_chip(&target),
            Err(DiscoveryFailure::InvalidJson)
        );
    }
    fixture.script("printf 'private fixture stderr' >&2; exit 9");
    for target in [fixture.local(), fixture.remote()] {
        assert_eq!(
            try_discover_tasks_for_target(&target),
            Err(DiscoveryFailure::Exit)
        );
        assert_eq!(
            chip::discover_tool_version_chip(&target),
            Err(DiscoveryFailure::Exit)
        );
    }
    fixture.script("printf 'null'");
    assert_eq!(
        chip::discover_tool_version_chip(&fixture.local()),
        Err(DiscoveryFailure::InvalidJson)
    );
}

#[test]
fn all_four_commands_cap_output_and_deadline() {
    let _guard = task_discovery_test_guard();
    reset();
    let fixture = Fixture::new("head -c 1100000 /dev/zero");
    for target in [fixture.local(), fixture.remote()] {
        assert_eq!(
            try_discover_tasks_for_target(&target),
            Err(DiscoveryFailure::OutputLimit)
        );
        assert_eq!(
            chip::discover_tool_version_chip(&target),
            Err(DiscoveryFailure::OutputLimit)
        );
    }
    fixture.script("exec sleep 30");
    for target in [fixture.local(), fixture.remote()] {
        let started = Instant::now();
        assert_eq!(
            try_discover_tasks_for_target(&target),
            Err(DiscoveryFailure::Timeout)
        );
        assert!(started.elapsed() < DISCOVERY_LEASE);
        let started = Instant::now();
        assert_eq!(
            chip::discover_tool_version_chip(&target),
            Err(DiscoveryFailure::Timeout)
        );
        assert!(started.elapsed() < DISCOVERY_LEASE);
    }
}

#[test]
fn discovery_pipe_deadline_and_retained_bytes_are_bounded() {
    let mut command = crate::child_process::command("sh");
    command.args(["-c", "(sleep 1) & exit 0"]);
    let started = Instant::now();
    assert_eq!(
        run_discovery_command(command, Duration::from_millis(50)),
        Err(DiscoveryFailure::Timeout)
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    let mut command = crate::child_process::command("sh");
    command.args([
        "-c",
        "head -c 1100000 /dev/zero; head -c 1100000 /dev/zero >&2",
    ]);
    let outcome =
        crate::tmux_process::run_command(command, DISCOVERY_TIMEOUT, DISCOVERY_OUTPUT_LIMIT);
    assert_eq!(outcome.stdout.len(), DISCOVERY_OUTPUT_LIMIT);
    assert_eq!(outcome.stderr.len(), DISCOVERY_OUTPUT_LIMIT);
    assert_eq!(
        discovery_output(outcome),
        Err(DiscoveryFailure::OutputLimit)
    );
}

fn expire_task_failure(target: &DiscoveryTarget) {
    let mut cache = cache::task_discovery_cache().lock().unwrap();
    if let Some(TaskDiscoveryCacheEntry::Failed { completed_at, .. }) =
        cache.entries.get_mut(&DiscoveryCacheKey::from(target))
    {
        *completed_at = Instant::now() - DISCOVERY_RETRY - Duration::from_millis(1);
    } else {
        panic!("expected failed task entry");
    }
}

fn expire_chip_failure(target: &DiscoveryTarget) {
    let mut cache = chip::tool_version_cache().lock().unwrap();
    if let Some(ToolVersionCacheEntry::Failed { completed_at, .. }) =
        cache.entries.get_mut(&chip::tool_version_cache_key(target))
    {
        *completed_at = Instant::now() - DISCOVERY_RETRY - Duration::from_millis(1);
    } else {
        panic!("expected failed chip entry");
    }
}

#[test]
fn task_failure_retry_and_stale_generation_cannot_replace_empty_success() {
    let _guard = task_discovery_test_guard();
    reset();
    let fixture = Fixture::new("printf '[]'");
    let target = fixture.local();
    let (request, first) = cache::prepare_task_discovery(&target);
    assert_eq!(request, TaskDiscoveryRequest::Start);
    assert_eq!(
        cache::prepare_task_discovery(&target).0,
        TaskDiscoveryRequest::Pending
    );
    cache::complete_task_discovery(&target, first, Err(DiscoveryFailure::InvalidJson));
    assert_eq!(
        cached_task_discovery(&target),
        CachedTaskDiscovery::Failed(DiscoveryFailure::InvalidJson)
    );
    assert_eq!(
        cache::prepare_task_discovery(&target).0,
        TaskDiscoveryRequest::UseCached
    );
    expire_task_failure(&target);
    let (request, second) = cache::prepare_task_discovery(&target);
    assert_eq!(request, TaskDiscoveryRequest::Start);
    assert_ne!(first, second);
    cache::complete_task_discovery(&target, first, Ok(Vec::new()));
    assert_eq!(cached_task_discovery(&target), CachedTaskDiscovery::Pending);
    cache::complete_task_discovery(&target, second, Ok(Vec::new()));
    cache::complete_task_discovery(&target, first, Err(DiscoveryFailure::Timeout));
    assert_eq!(
        cached_task_discovery(&target),
        CachedTaskDiscovery::Ready(Vec::new())
    );
    assert_eq!(
        cache::prepare_task_discovery(&target).0,
        TaskDiscoveryRequest::UseCached
    );
}

#[test]
fn chip_failure_retry_and_stale_generation_cannot_replace_empty_success() {
    let _guard = task_discovery_test_guard();
    reset();
    let fixture = Fixture::new("printf '[]'");
    let target = fixture.remote();
    let (request, first) = chip::prepare_tool_version_discovery(&target);
    assert_eq!(request, ToolVersionDiscoveryRequest::Start);
    assert_eq!(
        chip::prepare_tool_version_discovery(&target).0,
        ToolVersionDiscoveryRequest::Pending
    );
    chip::complete_tool_version_chip(&target, first, Err(DiscoveryFailure::InvalidJson));
    assert_eq!(
        tool_version_chip_text_for_target(&target).as_deref(),
        Some("discovery returned invalid JSON")
    );
    assert_eq!(
        chip::prepare_tool_version_discovery(&target).0,
        ToolVersionDiscoveryRequest::UseCached
    );
    expire_chip_failure(&target);
    let (request, second) = chip::prepare_tool_version_discovery(&target);
    assert_eq!(request, ToolVersionDiscoveryRequest::Start);
    assert_ne!(first, second);
    chip::complete_tool_version_chip(&target, first, Ok(Some("stale".into())));
    assert_eq!(chip::cached_tool_version_chip(&target), None);
    chip::complete_tool_version_chip(&target, second, Ok(None));
    chip::complete_tool_version_chip(&target, first, Err(DiscoveryFailure::Timeout));
    assert_eq!(chip::cached_tool_version_chip(&target), Some(None));
    assert_eq!(
        chip::prepare_tool_version_discovery(&target).0,
        ToolVersionDiscoveryRequest::UseCached
    );
}

#[test]
fn abandoned_task_and_chip_leases_expire_and_reject_completion() {
    let _guard = task_discovery_test_guard();
    reset();
    let fixture = Fixture::new("printf '[]'");
    let target = fixture.local();
    let (_, task_generation) = cache::prepare_task_discovery(&target);
    let (_, chip_generation) = chip::prepare_tool_version_discovery(&target);
    let started_at = Instant::now() - DISCOVERY_LEASE - Duration::from_millis(1);
    cache::task_discovery_cache()
        .lock()
        .unwrap()
        .entries
        .insert(
            DiscoveryCacheKey::from(&target),
            TaskDiscoveryCacheEntry::Pending {
                generation: task_generation,
                started_at,
            },
        );
    chip::tool_version_cache().lock().unwrap().entries.insert(
        chip::tool_version_cache_key(&target),
        ToolVersionCacheEntry::Pending {
            generation: chip_generation,
            started_at,
        },
    );
    assert_eq!(
        cached_task_discovery(&target),
        CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
    );
    assert_eq!(
        chip::cached_tool_version_chip(&target),
        Some(Some("discovery timed out".into()))
    );
    cache::complete_task_discovery(&target, task_generation, Ok(Vec::new()));
    chip::complete_tool_version_chip(&target, chip_generation, Ok(None));
    assert_eq!(
        cached_task_discovery(&target),
        CachedTaskDiscovery::Failed(DiscoveryFailure::Timeout)
    );
    assert_eq!(
        chip::cached_tool_version_chip(&target),
        Some(Some("discovery timed out".into()))
    );
    expire_task_failure(&target);
    expire_chip_failure(&target);
    assert_eq!(
        cache::prepare_task_discovery(&target).0,
        TaskDiscoveryRequest::Start
    );
    assert_eq!(
        chip::prepare_tool_version_discovery(&target).0,
        ToolVersionDiscoveryRequest::Start
    );
}

#[test]
fn background_failures_settle_pending_and_allow_real_retry() {
    let _guard = task_discovery_test_guard();
    reset();
    for (script, reason) in [
        ("exec sleep 30", DiscoveryFailure::Timeout),
        ("head -c 1100000 /dev/zero", DiscoveryFailure::OutputLimit),
        ("printf '{broken'", DiscoveryFailure::InvalidJson),
    ] {
        let fixture = Fixture::new(script);
        let task_target = fixture.local();
        let chip_target = fixture.remote();
        assert_eq!(
            spawn_task_discovery(task_target.clone()),
            TaskDiscoveryRequest::Start
        );
        assert_eq!(tool_version_chip_text_for_target(&chip_target), None);
        let deadline = Instant::now() + DISCOVERY_LEASE;
        loop {
            if matches!(
                cached_task_discovery(&task_target),
                CachedTaskDiscovery::Failed(current) if current == reason
            ) && chip::cached_tool_version_chip(&chip_target)
                == Some(Some(reason.label().into()))
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        expire_task_failure(&task_target);
        expire_chip_failure(&chip_target);
        fixture.script("printf '[]'");
        assert_eq!(
            spawn_task_discovery(task_target.clone()),
            TaskDiscoveryRequest::Start
        );
        assert_eq!(tool_version_chip_text_for_target(&chip_target), None);
        let deadline = Instant::now() + DISCOVERY_LEASE;
        loop {
            if cached_task_discovery(&task_target) == CachedTaskDiscovery::Ready(Vec::new())
                && chip::cached_tool_version_chip(&chip_target) == Some(None)
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
