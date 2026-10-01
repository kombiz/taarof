//! Bounded, injectable observation rounds; all commands retain the D08 runner.
use crate::tmux::{TmuxPaneInfo, TmuxTarget};

pub(super) const TARGET_CONCURRENCY: usize = 4;

pub(super) fn bounded_map<T: Sync, R: Send>(
    inputs: &[T],
    observe: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::with_capacity(inputs.len()));
    std::thread::scope(|scope| {
        for _ in 0..inputs.len().min(TARGET_CONCURRENCY) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(input) = inputs.get(index) else {
                    break;
                };
                let result = observe(input);
                results
                    .lock()
                    .expect("observation results poisoned")
                    .push((index, result));
            });
        }
    });
    let mut results = results.into_inner().expect("observation results poisoned");
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

pub(super) type PaneRequest = (u32, u32, TmuxTarget, String, crate::pane::TmuxBacking);
pub(super) type PaneResult = (PaneRequest, Result<TmuxPaneInfo, String>);
pub(super) type HostResult = (u32, String, Result<crate::host::HostStatus, String>);

pub(super) fn accept_pane_result(
    current: &crate::pane::TmuxBacking,
    captured: &crate::pane::TmuxBacking,
    result: &mut Result<TmuxPaneInfo, String>,
) -> bool {
    if !current.same_execution_target(captured) {
        return false;
    }
    // Even a backing without a saved identity must not acquire a replacement
    // after it has observed the original session's creation identity.
    let expected = current.expected_generation.as_ref();
    let observed = current.pane_info.value();
    let replacement = result.as_ref().is_ok_and(|info| {
        if let Some(expected) = expected {
            !super::restore::tmux_matches_expected_generation(expected, info)
        } else if let Some(observed) = observed {
            info.session_id != observed.session_id
                || info.session_created != observed.session_created
                || info.continuity_id != observed.continuity_id
        } else {
            false
        }
    });
    if replacement {
        *result =
            Err("exact saved tmux target no longer exists; same-name replacement refused".into());
    }
    true
}

pub(super) fn observe_hosts(
    targets: Vec<(u32, String)>,
    run: impl Fn(&[String]) -> Result<String, String> + Sync,
) -> Vec<HostResult> {
    let mut hosts: Vec<String> = targets.iter().map(|(_, host)| host.clone()).collect();
    hosts.sort();
    hosts.dedup();
    let observations = bounded_map(&hosts, |host| {
        let status = run(&crate::host::probe_status_command(host)).and_then(|output| {
            crate::host::parse_probe_output(&output)
                .ok_or_else(|| "host probe returned invalid status output".to_string())
        });
        (host.clone(), status)
    });
    targets
        .into_iter()
        .map(|(ws_id, host)| {
            let status = observations
                .iter()
                .find(|(target, _)| target == &host)
                .expect("collected host")
                .1
                .clone();
            (ws_id, host, status)
        })
        .collect()
}

pub(super) fn observe_panes(
    requests: Vec<PaneRequest>,
    run: impl Fn(&[String]) -> Result<String, String> + Sync,
) -> Vec<PaneResult> {
    let mut groups: Vec<(TmuxTarget, Vec<PaneRequest>)> = Vec::new();
    for request in requests {
        if let Some((_, panes)) = groups.iter_mut().find(|(target, _)| *target == request.2) {
            panes.push(request);
        } else {
            groups.push((request.2.clone(), vec![request]));
        }
    }
    bounded_map(&groups, |(target, panes)| {
        let output = run(&crate::tmux::list_panes_info_command(target));
        panes
            .iter()
            .map(|request| {
                let result = output
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|output| crate::tmux::parse_selected_pane_info(output, &request.3));
                (request.clone(), result)
            })
            .collect::<Vec<_>>()
    })
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
