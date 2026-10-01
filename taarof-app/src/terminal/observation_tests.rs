use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn backing(target: TmuxTarget, name: &str) -> crate::pane::TmuxBacking {
    crate::pane::TmuxBacking {
        session_name: name.into(),
        target,
        expected_generation: None,
        pane_info: Default::default(),
    }
}

fn request(id: u32, target: TmuxTarget, name: &str) -> PaneRequest {
    let backing = backing(target.clone(), name);
    (1, id, target, name.into(), backing)
}

fn row(name: &str, window: u32, pane: u32, identity: &str) -> String {
    let sep = crate::tmux::PANE_INFO_SEPARATOR;
    format!("{name}{sep}{window}{sep}{pane}{sep}vim{sep}/tmp/project{sep}42{sep}120{sep}40{sep}{identity}{sep}123{sep}{}", "11".repeat(16))
}

#[test]
fn metadata_round_one_query_for_shared_exact_target_preserves_selected_eight_fields() {
    let calls = AtomicUsize::new(0);
    let results = observe_panes(
        vec![
            request(1, TmuxTarget::Local, "a"),
            request(2, TmuxTarget::Local, "b"),
        ],
        |argv| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(&argv[..4], &["tmux", "list-panes", "-a", "-F"]);
            Ok([
                row("a", 0, 1, "$9"),
                row("a", 1, 0, "$9"),
                row("a", 1, 1, "$1"),
                row("b", 1, 1, "$2"),
            ]
            .join("\n"))
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let info = results[0].1.as_ref().unwrap();
    assert_eq!(
        (
            info.current_command.as_str(),
            info.cwd.as_str(),
            info.pid,
            info.width,
            info.height,
            info.session_id.as_str(),
            info.session_created,
            info.continuity_id.as_deref()
        ),
        (
            "vim",
            "/tmp/project",
            42,
            120,
            40,
            "$1",
            123,
            Some("11111111111111111111111111111111")
        )
    );
    assert_eq!(results[1].1.as_ref().unwrap().session_id, "$2");
}

#[test]
fn metadata_round_missing_malformed_and_ambiguous_rows_remain_per_pane_errors() {
    let sep = crate::tmux::PANE_INFO_SEPARATOR;
    let output = [
        row("good", 1, 1, "$1"),
        format!("bad{sep}1{sep}1{sep}invalid"),
        row("duplicate", 1, 1, "$1"),
        row("duplicate", 1, 1, "$1"),
    ]
    .join("\n");
    let results = observe_panes(
        ["good", "bad", "missing", "duplicate"]
            .iter()
            .enumerate()
            .map(|(id, name)| request(id as u32, TmuxTarget::Local, name))
            .collect(),
        |_| Ok(output.clone()),
    );
    assert!(results[0].1.is_ok());
    assert!(results[1].1.as_ref().unwrap_err().contains("invalid"));
    assert!(results[2].1.as_ref().unwrap_err().contains("missing"));
    assert!(results[3].1.as_ref().unwrap_err().contains("ambiguous"));
}

#[test]
fn metadata_round_exact_remote_routes_bounded_concurrency_and_slow_failure() {
    let active = AtomicUsize::new(0);
    let max = AtomicUsize::new(0);
    let calls = AtomicUsize::new(0);
    let requests = (0..9)
        .map(|id| {
            request(
                id,
                TmuxTarget::Remote {
                    ssh_target: format!("host-{id}.ts"),
                },
                "a",
            )
        })
        .collect();
    let started = std::time::Instant::now();
    let results = observe_panes(requests, |argv| {
        calls.fetch_add(1, Ordering::SeqCst);
        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[4], "ConnectTimeout=5");
        std::thread::sleep(std::time::Duration::from_millis(20));
        active.fetch_sub(1, Ordering::SeqCst);
        if argv[5] == "host-0.ts" {
            Err("fixture deadline exceeded".into())
        } else {
            Ok(row("a", 1, 1, "$1"))
        }
    });
    eprintln!(
        "D09 fixture: queries={} max_concurrency={} elapsed_ms={}",
        calls.load(Ordering::SeqCst),
        max.load(Ordering::SeqCst),
        started.elapsed().as_millis()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 9);
    assert!(max.load(Ordering::SeqCst) > 1);
    assert!(max.load(Ordering::SeqCst) <= TARGET_CONCURRENCY);
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_err()).count(),
        1
    );
}

#[test]
fn metadata_apply_rejects_changed_backing_and_same_name_generation_aba() {
    let mut current = backing(TmuxTarget::Local, "a");
    let old = crate::tmux::parse_selected_pane_info(&row("a", 1, 1, "$1"), "a").unwrap();
    current.pane_info.record_success(old.clone());
    let captured = current.clone();
    let mut replacement = old.clone();
    replacement.session_created += 1;
    let mut result = Ok(replacement);
    assert!(accept_pane_result(&current, &captured, &mut result));
    assert!(result.is_err());
    let mut replacement = old.clone();
    replacement.continuity_id = Some("22".repeat(16));
    let mut result = Ok(replacement);
    assert!(accept_pane_result(&current, &captured, &mut result));
    assert!(result.is_err());
    current.target = TmuxTarget::Remote {
        ssh_target: "other.ts".into(),
    };
    assert!(!accept_pane_result(&current, &captured, &mut Ok(old)));
}

#[test]
fn host_round_deduplicates_exact_hosts_and_preserves_failure_fanout() {
    let calls = AtomicUsize::new(0);
    let results = observe_hosts(
        vec![(1, "a.ts".into()), (2, "a.ts".into()), (3, "b.ts".into())],
        |argv| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(format!("fixture failure {}", argv[5]))
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].2, results[1].2);
    assert_ne!(results[0].2, results[2].2);
}

#[test]
fn dashboard_round_serializes_each_target_and_bounds_existing_queries() {
    let targets = (0..9)
        .map(|id| TmuxTarget::Remote {
            ssh_target: format!("host-{id}.ts"),
        })
        .collect::<Vec<_>>();
    let detached = targets
        .iter()
        .flat_map(|target| {
            [
                ("a".into(), target.clone(), std::time::Instant::now()),
                ("b".into(), target.clone(), std::time::Instant::now()),
            ]
        })
        .collect::<Vec<_>>();
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let calls = AtomicUsize::new(0);
    let target_active = std::sync::Mutex::new(std::collections::HashSet::new());
    let run = |argv: &[String]| {
        let host = argv[5].clone();
        assert!(
            target_active.lock().unwrap().insert(host.clone()),
            "same target commands overlap"
        );
        calls.fetch_add(1, Ordering::SeqCst);
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(count, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(10));
        active.fetch_sub(1, Ordering::SeqCst);
        assert!(target_active.lock().unwrap().remove(&host));
        if host == "host-0.ts" {
            Err("fixture deadline".into())
        } else {
            Ok(String::new())
        }
    };
    let started = std::time::Instant::now();
    let (commands, sessions, errors) =
        super::super::attach::observe_dashboard(&detached, &targets, run, run);
    eprintln!(
        "D09 dashboard fixture: queries={} max_concurrency={} elapsed_ms={}",
        calls.load(Ordering::SeqCst),
        maximum.load(Ordering::SeqCst),
        started.elapsed().as_millis()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 27);
    assert!(maximum.load(Ordering::SeqCst) > 1);
    assert!(maximum.load(Ordering::SeqCst) <= TARGET_CONCURRENCY);
    assert_eq!(commands.len(), 18);
    assert_eq!(
        commands
            .iter()
            .filter(|(_, result)| result.is_err())
            .count(),
        2
    );
    assert_eq!(sessions.len(), 8);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0.as_ref(), Some(&targets[0]));
}

#[test]
fn metadata_apply_saved_identity_rejection_keeps_prior_value_stale() {
    let info = crate::tmux::parse_selected_pane_info(&row("a", 1, 1, "$1"), "a").unwrap();
    let mut current = backing(TmuxTarget::Local, "a");
    current.expected_generation = Some(crate::session::SavedTmuxIdentity {
        session_id: info.session_id.clone(),
        session_created: info.session_created,
        continuity_id: info.continuity_id.clone().unwrap(),
    });
    current.pane_info.record_success(info.clone());
    let captured = current.clone();
    let mut other = info.clone();
    other.session_id = "$2".into();
    let mut result = Ok(other);
    assert!(accept_pane_result(&current, &captured, &mut result));
    current.pane_info.record_failure(result.unwrap_err());
    assert_eq!(current.pane_info.state, crate::probe::ProbeState::Stale);
    assert_eq!(current.pane_info.value(), Some(&info));
    current
        .expected_generation
        .as_mut()
        .unwrap()
        .session_created += 1;
    assert!(!accept_pane_result(&current, &captured, &mut Ok(info)));
}

#[test]
fn observation_single_flight_skips_overlapping_round_and_releases_after_error_and_stale_apply() {
    let flight = crate::runtime_probe::ProbeInFlight::default();
    let guard = flight.try_begin().unwrap();
    let calls = AtomicUsize::new(0);
    let results = std::thread::scope(|scope| {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = &calls;
        let worker = scope.spawn(move || {
            observe_panes(vec![request(1, TmuxTarget::Local, "a")], |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Err("fixture saturation".into())
            })
        });
        entered_rx.recv().unwrap();
        assert!(flight.try_begin().is_none());
        release_tx.send(()).unwrap();
        worker.join().unwrap()
    });
    assert!(results[0].1.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(guard);
    let guard = flight.try_begin().unwrap();
    let current = backing(TmuxTarget::Local, "b");
    let captured = backing(TmuxTarget::Local, "a");
    assert!(!accept_pane_result(
        &current,
        &captured,
        &mut Err("late".into())
    ));
    drop(guard);
    assert!(flight.try_begin().is_some());
}

#[test]
fn observation_single_flight_success_empty_round_and_independent_pollers_release() {
    let pollers = [
        crate::runtime_probe::ProbeInFlight::default(),
        crate::runtime_probe::ProbeInFlight::default(),
        crate::runtime_probe::ProbeInFlight::default(),
    ];
    let guards = pollers
        .iter()
        .map(|poller| poller.try_begin().unwrap())
        .collect::<Vec<_>>();
    assert!(pollers.iter().all(|poller| poller.try_begin().is_none()));
    let results = observe_panes(vec![request(1, TmuxTarget::Local, "a")], |_| {
        Ok(row("a", 1, 1, "$1"))
    });
    assert!(results[0].1.is_ok());
    assert!(observe_panes(Vec::new(), |_| panic!("empty round must not query")).is_empty());
    drop(guards);
    assert!(pollers.iter().all(|poller| poller.try_begin().is_some()));
}

#[test]
fn observation_single_flight_worker_failure_unwinds_without_wedging_admission() {
    let flight = crate::runtime_probe::ProbeInFlight::default();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = flight.try_begin().unwrap();
        observe_panes(vec![request(1, TmuxTarget::Local, "a")], |_| {
            panic!("fixture worker saturation")
        });
    }));
    assert!(failed.is_err());
    assert!(flight.try_begin().is_some());
}
