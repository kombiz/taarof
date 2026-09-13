use std::cell::{Cell, OnceCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;

use crate::{
    agents::{self, AgentStatus, ListenTableProbe, ProcessFact},
    pane::PaneProcessState,
    probe::ProbeState,
};

pub(crate) const PROBE_TTL_MS: u64 = 3_000;
const AGENT_SCAN_MAX_PROCESSES: usize = 256;

type PaneKey = (u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeProbeState {
    Ok,
    Absent,
    Partial,
    Stale,
    Failed,
    Unknown,
}

impl RuntimeProbeState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Absent => "absent",
            Self::Partial => "partial",
            Self::Stale => "stale",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn is_degraded(self) -> bool {
        matches!(
            self,
            Self::Partial | Self::Stale | Self::Failed | Self::Unknown
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeProbeTruth {
    pub state: RuntimeProbeState,
    pub process_state: ProbeState,
    pub ports_state: ProbeState,
    pub process_observed_at_unix_ms: Option<u64>,
    pub ports_observed_at_unix_ms: Option<u64>,
    pub process_age_ms: Option<u64>,
    pub ports_age_ms: Option<u64>,
    pub process_error: Option<String>,
    pub ports_error: Option<String>,
    pub checked_at_unix_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeProbeWorkerFailure {
    Panicked,
    Cancelled,
}

impl RuntimeProbeWorkerFailure {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Panicked => "runtime probe worker panicked",
            Self::Cancelled => "runtime probe worker cancelled",
        }
    }
}

pub(crate) fn runtime_probe_truth(
    snapshot: Option<&RuntimeProbeSnapshot>,
    tab_pids: &[(u32, Vec<i32>)],
    pane_pids: &[(u32, u32, i32)],
    now_ms: u64,
    ttl_ms: u64,
) -> RuntimeProbeTruth {
    let roots_absent = pane_pids.is_empty() && tab_pids.iter().all(|(_, pids)| pids.is_empty());
    if roots_absent {
        return RuntimeProbeTruth {
            state: RuntimeProbeState::Absent,
            process_state: ProbeState::Unknown,
            ports_state: ProbeState::Unknown,
            process_observed_at_unix_ms: None,
            ports_observed_at_unix_ms: None,
            process_age_ms: None,
            ports_age_ms: None,
            process_error: None,
            ports_error: None,
            checked_at_unix_ms: snapshot.map(|snapshot| snapshot.probed_at_unix_ms),
        };
    }

    let Some(snapshot) = snapshot else {
        return RuntimeProbeTruth {
            state: RuntimeProbeState::Unknown,
            process_state: ProbeState::Unknown,
            ports_state: ProbeState::Unknown,
            process_observed_at_unix_ms: None,
            ports_observed_at_unix_ms: None,
            process_age_ms: None,
            ports_age_ms: None,
            process_error: None,
            ports_error: None,
            checked_at_unix_ms: None,
        };
    };

    let process_state = snapshot.process_probe_state_for(tab_pids, pane_pids, now_ms, ttl_ms);
    let ports_state = snapshot.ports_probe_state_for(tab_pids, pane_pids, now_ms, ttl_ms);
    let state = match (process_state, ports_state) {
        (ProbeState::Ok, ProbeState::Ok) => RuntimeProbeState::Ok,
        (ProbeState::Stale, ProbeState::Stale) => RuntimeProbeState::Stale,
        (ProbeState::Error, ProbeState::Error)
        | (ProbeState::Error, ProbeState::Unknown)
        | (ProbeState::Unknown, ProbeState::Error) => RuntimeProbeState::Failed,
        (ProbeState::Unknown, ProbeState::Unknown) => RuntimeProbeState::Unknown,
        _ => RuntimeProbeState::Partial,
    };

    RuntimeProbeTruth {
        state,
        process_state,
        ports_state,
        process_observed_at_unix_ms: snapshot.process_observed_at_unix_ms,
        ports_observed_at_unix_ms: snapshot.ports_observed_at_unix_ms,
        process_age_ms: snapshot
            .process_observed_at_unix_ms
            .map(|observed| now_ms.saturating_sub(observed)),
        ports_age_ms: snapshot
            .ports_observed_at_unix_ms
            .map(|observed| now_ms.saturating_sub(observed)),
        process_error: snapshot.process_error.clone(),
        ports_error: snapshot.ports_error.clone(),
        checked_at_unix_ms: Some(snapshot.probed_at_unix_ms),
    }
}

pub(crate) fn runtime_probe_truth_for_state(
    state: &crate::AppState,
    now_ms: u64,
    ttl_ms: u64,
) -> RuntimeProbeTruth {
    let mut tab_pids = Vec::new();
    let mut pane_pids = Vec::new();
    for tab in state.all_tabs() {
        tab_pids.push((tab.id, tab.panes.collect_pids()));
        pane_pids.extend(tab.panes.leaves().into_iter().filter_map(|leaf| {
            pane_process_root(leaf, now_ms).map(|pid| (tab.id, leaf.pane_id, pid))
        }));
    }
    runtime_probe_truth(
        state.runtime_probe.as_ref(),
        &tab_pids,
        &pane_pids,
        now_ms,
        ttl_ms,
    )
}

/// Use the local tmux pane shell, never its attach-client PID, only while the
/// same-user tmux probe is healthy and current. Remote PIDs are not local /proc.
pub(crate) fn pane_process_root(leaf: &crate::pane::PaneLeaf, now_ms: u64) -> Option<i32> {
    local_pane_process_root(leaf.shell_pid, leaf.tmux_backing.as_ref(), now_ms)
}
fn local_pane_process_root(
    shell_pid: Option<i32>,
    backing: Option<&crate::pane::TmuxBacking>,
    now_ms: u64,
) -> Option<i32> {
    let shell_pid = shell_pid.filter(|pid| *pid > 0)?;
    backing
        .and_then(|backing| fresh_local_tmux_pid(backing, now_ms))
        .or(Some(shell_pid))
}

/// Shared metadata authority for process-root selection and Attach projection.
pub(crate) fn fresh_local_tmux_pid(backing: &crate::pane::TmuxBacking, now_ms: u64) -> Option<i32> {
    if backing.target != crate::tmux::TmuxTarget::Local {
        return None;
    }
    let probe = &backing.pane_info;
    let fresh = probe
        .observed_at_unix_ms
        .and_then(|at| now_ms.checked_sub(at))
        .is_some_and(|age| age <= crate::tmux::TMUX_METADATA_TTL_MS);
    (probe.state == ProbeState::Ok && fresh)
        .then(|| {
            probe
                .value
                .as_ref()
                .filter(|info| info.pid > 0)
                .map(|info| info.pid)
        })
        .flatten()
}

pub(crate) fn runtime_process_truth_is_fresh(state: &crate::AppState) -> bool {
    matches!(
        runtime_probe_truth_for_state(state, crate::events::unix_time_ms(), PROBE_TTL_MS)
            .process_state,
        ProbeState::Ok
    )
}

pub(crate) trait RuntimeProbeSource {
    fn probe_process_root(&self, pid: i32) -> Result<(Vec<i32>, Option<String>), String> {
        Ok((self.child_pids(pid), self.process_comm(pid)))
    }
    /// None means executable identity is unproven; never fall back to comm.
    fn verified_executable_name(&self, _pid: i32) -> Option<String> {
        None
    }
    fn child_pids(&self, pid: i32) -> Vec<i32>;
    fn process_comm(&self, pid: i32) -> Option<String>;
    fn process_cmdline(&self, pid: i32) -> Option<Vec<String>>;
    fn process_cwd(&self, pid: i32) -> Option<String>;
    fn socket_inodes(&self, pid: i32) -> Vec<u64>;
    fn listen_table(&self) -> ListenTableProbe;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcProbeSource;

impl RuntimeProbeSource for ProcProbeSource {
    fn verified_executable_name(&self, pid: i32) -> Option<String> {
        let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        // Native launchers can resolve to versioned filenames. A named argv[0]
        // is usable only when its actual file resolves to this process's exe.
        let argv = agents::get_process_cmdline(pid)?;
        if let Some(invocation) = argv.first() {
            let candidates: Vec<_> = if invocation.starts_with('/') {
                vec![std::path::PathBuf::from(invocation)]
            } else if !invocation.contains('/') {
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .map(|p| p.join(invocation))
                    .collect()
            } else {
                Vec::new()
            };
            if candidates
                .iter()
                .any(|path| std::fs::canonicalize(path).is_ok_and(|path| path == executable))
            {
                return Some(invocation.clone());
            }
        }
        executable.into_os_string().into_string().ok()
    }

    fn probe_process_root(&self, pid: i32) -> Result<(Vec<i32>, Option<String>), String> {
        agents::try_get_root_process_facts(pid)
    }

    fn child_pids(&self, pid: i32) -> Vec<i32> {
        agents::get_child_pids(pid)
    }

    fn process_comm(&self, pid: i32) -> Option<String> {
        agents::get_process_comm(pid)
    }

    fn process_cmdline(&self, pid: i32) -> Option<Vec<String>> {
        agents::get_process_cmdline(pid)
    }

    fn process_cwd(&self, pid: i32) -> Option<String> {
        agents::get_process_cwd(pid)
    }

    fn socket_inodes(&self, pid: i32) -> Vec<u64> {
        agents::get_socket_inodes(pid)
    }

    fn listen_table(&self) -> ListenTableProbe {
        agents::try_build_listen_table()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeProbeSnapshot {
    pub probed_at_unix_ms: u64,
    pub process_observed_at_unix_ms: Option<u64>,
    pub ports_observed_at_unix_ms: Option<u64>,
    pub tab_pids: BTreeMap<u32, Vec<i32>>,
    pub pane_pids: BTreeMap<PaneKey, i32>,
    pub pane_process_states: HashMap<PaneKey, PaneProcessState>,
    pub pane_agents: HashMap<PaneKey, AgentStatus>,
    /// Strict executable + explicit session identity; advisory matches never enter.
    pub pane_exact_agents: HashMap<PaneKey, AgentStatus>,
    pub tab_agents: HashMap<u32, AgentStatus>,
    pub tab_ports: HashMap<u32, Vec<u16>>,
    pub process_probe: ProbeState,
    pub ports_probe: ProbeState,
    pub process_error: Option<String>,
    pub ports_error: Option<String>,
}

impl RuntimeProbeSnapshot {
    pub(crate) fn retain_tabs(&mut self, live: &HashSet<u32>) {
        self.tab_pids.retain(|tab, _| live.contains(tab));
        self.pane_pids.retain(|(tab, _), _| live.contains(tab));
        self.pane_process_states
            .retain(|(tab, _), _| live.contains(tab));
        self.pane_agents.retain(|(tab, _), _| live.contains(tab));
        self.pane_exact_agents
            .retain(|(tab, _), _| live.contains(tab));
        self.tab_agents.retain(|tab, _| live.contains(tab));
        self.tab_ports.retain(|tab, _| live.contains(tab));
    }

    pub(crate) fn build<S: RuntimeProbeSource + ?Sized>(
        source: &S,
        now_ms: u64,
        tab_pids: &[(u32, Vec<i32>)],
        pane_pids: &[(u32, u32, i32)],
    ) -> Self {
        let mut cache = ProcessTreeCache::default();
        let roots = tab_pids
            .iter()
            .flat_map(|(_, pids)| pids.iter().copied())
            .chain(pane_pids.iter().map(|(_, _, pid)| *pid))
            .collect::<HashSet<_>>();
        for pid in roots {
            cache.validate_root(source, pid);
        }
        let mut pane_process_states = HashMap::new();
        let mut pane_agents = HashMap::new();
        let mut pane_exact_agents = HashMap::new();

        for (tab_id, pane_id, pid) in pane_pids {
            pane_process_states.insert(
                (*tab_id, *pane_id),
                cache.pane_process_state(source, *pid, now_ms),
            );
            pane_agents.insert(
                (*tab_id, *pane_id),
                cache.agent_status_for_root(source, *pid),
            );
            if let Some(status) = cache.exact_agent_status_for_root(source, *pid) {
                pane_exact_agents.insert((*tab_id, *pane_id), status);
            }
        }

        let mut tab_agents = HashMap::new();
        for (tab_id, pids) in tab_pids {
            let mut found = AgentStatus::default();
            for pid in pids {
                let status = cache.agent_status_for_root(source, *pid);
                if status.running {
                    found = status;
                    break;
                }
            }
            tab_agents.insert(*tab_id, found);
        }

        let (ports_probe, ports_error, tab_ports) = match source.listen_table() {
            ListenTableProbe::Complete(listen_table) => (
                ProbeState::Ok,
                None,
                cache.tab_ports(source, tab_pids, &listen_table),
            ),
            ListenTableProbe::Partial {
                table: listen_table,
                error,
            } => (
                ProbeState::Error,
                Some(bounded_probe_reason(error)),
                cache.tab_ports(source, tab_pids, &listen_table),
            ),
            ListenTableProbe::Failed { error } => (
                ProbeState::Error,
                Some(bounded_probe_reason(error)),
                HashMap::new(),
            ),
        };

        let process_error = cache.process_error.take().map(bounded_probe_reason);
        let process_probe = if process_error.is_some() {
            ProbeState::Error
        } else {
            ProbeState::Ok
        };

        Self {
            probed_at_unix_ms: now_ms,
            process_observed_at_unix_ms: matches!(process_probe, ProbeState::Ok).then_some(now_ms),
            ports_observed_at_unix_ms: matches!(ports_probe, ProbeState::Ok).then_some(now_ms),
            tab_pids: tab_pid_key(tab_pids),
            pane_pids: pane_pid_key(pane_pids),
            pane_process_states,
            pane_agents,
            pane_exact_agents,
            tab_agents,
            tab_ports,
            process_probe,
            ports_probe,
            process_error,
            ports_error,
        }
    }

    pub(crate) fn worker_failure(
        now_ms: u64,
        tab_pids: &[(u32, Vec<i32>)],
        pane_pids: &[(u32, u32, i32)],
        failure: RuntimeProbeWorkerFailure,
    ) -> Self {
        let reason = failure.reason().to_string();
        Self {
            probed_at_unix_ms: now_ms,
            process_observed_at_unix_ms: None,
            ports_observed_at_unix_ms: None,
            tab_pids: tab_pid_key(tab_pids),
            pane_pids: pane_pid_key(pane_pids),
            pane_process_states: HashMap::new(),
            pane_agents: HashMap::new(),
            pane_exact_agents: HashMap::new(),
            tab_agents: HashMap::new(),
            tab_ports: HashMap::new(),
            process_probe: ProbeState::Error,
            ports_probe: ProbeState::Error,
            process_error: Some(reason.clone()),
            ports_error: Some(reason),
        }
    }

    pub(crate) fn reconcile(mut self, previous: Option<&Self>) -> Self {
        if !matches!(self.process_probe, ProbeState::Ok) {
            self.pane_exact_agents.clear();
            if let Some(previous) = previous.filter(|previous| {
                previous.process_observed_at_unix_ms.is_some()
                    && (self
                        .pane_pids
                        .iter()
                        .any(|(key, pid)| previous.pane_pids.get(key) == Some(pid))
                        || self.tab_pids.iter().any(|(tab_id, pids)| {
                            !pids.is_empty() && previous.tab_pids.get(tab_id) == Some(pids)
                        }))
            }) {
                self.process_probe = ProbeState::Stale;
                self.process_observed_at_unix_ms = previous.process_observed_at_unix_ms;
                self.pane_process_states = previous
                    .pane_process_states
                    .iter()
                    .filter(|(key, _)| self.pane_pids.get(key) == previous.pane_pids.get(key))
                    .map(|(key, state)| (*key, state.clone()))
                    .collect();
                self.pane_agents = previous
                    .pane_agents
                    .iter()
                    .filter(|(key, _)| self.pane_pids.get(key) == previous.pane_pids.get(key))
                    .map(|(key, status)| (*key, status.clone()))
                    .collect();
                self.tab_agents = previous
                    .tab_agents
                    .iter()
                    .filter(|(tab_id, _)| {
                        self.tab_pids.get(tab_id) == previous.tab_pids.get(tab_id)
                    })
                    .map(|(tab_id, status)| (*tab_id, status.clone()))
                    .collect();
            } else {
                self.process_observed_at_unix_ms = None;
                self.pane_process_states.clear();
                self.pane_agents.clear();
                self.tab_agents.clear();
            }
        }

        if !matches!(self.ports_probe, ProbeState::Ok) {
            let partial_ports = std::mem::take(&mut self.tab_ports);
            if let Some(previous) = previous.filter(|previous| {
                previous.ports_observed_at_unix_ms.is_some()
                    && self.tab_pids.iter().any(|(tab_id, pids)| {
                        !pids.is_empty() && previous.tab_pids.get(tab_id) == Some(pids)
                    })
            }) {
                self.ports_probe = ProbeState::Stale;
                self.ports_observed_at_unix_ms = previous.ports_observed_at_unix_ms;
                self.tab_ports = previous
                    .tab_ports
                    .iter()
                    .filter(|(tab_id, _)| {
                        self.tab_pids.get(tab_id) == previous.tab_pids.get(tab_id)
                    })
                    .map(|(tab_id, ports)| (*tab_id, ports.clone()))
                    .collect();
                for (tab_id, ports) in partial_ports {
                    let merged = self.tab_ports.entry(tab_id).or_default();
                    merged.extend(ports);
                    merged.sort_unstable();
                    merged.dedup();
                }
            } else {
                self.ports_observed_at_unix_ms = None;
                self.tab_ports = partial_ports;
            }
        }

        self
    }

    /// True when `other` would render the same sidebar/dashboard UI as `self`,
    /// ignoring the probe tick timestamps. Used to skip `refresh_all_tab_rows`
    /// on ticks whose only delta is the clock. Every UI-relevant field is
    /// compared; `probed_at_unix_ms` and the per-pane `updated_at_unix_ms` are
    /// excluded because they advance every tick without changing what is drawn.
    pub(crate) fn renders_same_as(&self, other: &Self) -> bool {
        self.tab_pids == other.tab_pids
            && self.pane_pids == other.pane_pids
            && self.tab_agents == other.tab_agents
            && self.pane_agents == other.pane_agents
            && self.pane_exact_agents == other.pane_exact_agents
            && self.tab_ports == other.tab_ports
            && self.process_probe == other.process_probe
            && self.ports_probe == other.ports_probe
            && self.process_error == other.process_error
            && self.ports_error == other.ports_error
            && pane_process_states_render_same(
                &self.pane_process_states,
                &other.pane_process_states,
            )
    }

    pub(crate) fn is_fresh_for(
        &self,
        tab_pids: &[(u32, Vec<i32>)],
        pane_pids: &[(u32, u32, i32)],
        now_ms: u64,
        ttl_ms: u64,
    ) -> bool {
        now_ms.saturating_sub(self.probed_at_unix_ms) <= ttl_ms
            && self.tab_pids == tab_pid_key(tab_pids)
            && self.pane_pids == pane_pid_key(pane_pids)
    }

    pub(crate) fn process_probe_state_for(
        &self,
        tab_pids: &[(u32, Vec<i32>)],
        pane_pids: &[(u32, u32, i32)],
        now_ms: u64,
        ttl_ms: u64,
    ) -> ProbeState {
        effective_probe_state(
            self.process_probe,
            self.is_fresh_for(tab_pids, pane_pids, now_ms, ttl_ms),
        )
    }

    pub(crate) fn ports_probe_state_for(
        &self,
        tab_pids: &[(u32, Vec<i32>)],
        pane_pids: &[(u32, u32, i32)],
        now_ms: u64,
        ttl_ms: u64,
    ) -> ProbeState {
        effective_probe_state(
            self.ports_probe,
            self.is_fresh_for(tab_pids, pane_pids, now_ms, ttl_ms),
        )
    }
}

const PROBE_REASON_MAX_CHARS: usize = 160;

fn bounded_probe_reason(reason: String) -> String {
    let normalized = reason
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() <= PROBE_REASON_MAX_CHARS {
        return normalized;
    }
    let mut bounded = normalized
        .chars()
        .take(PROBE_REASON_MAX_CHARS.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    bounded
}

/// Compare two pane process-state maps for render equality, ignoring the
/// per-pane `updated_at_unix_ms` timestamp (which advances every tick).
/// `PaneProcessState` has no `PartialEq`, so only the rendered fields are
/// checked: child-process presence, the remote-shell flag, and the ssh command.
fn pane_process_states_render_same(
    a: &HashMap<PaneKey, PaneProcessState>,
    b: &HashMap<PaneKey, PaneProcessState>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(key, av)| {
        b.get(key).is_some_and(|bv| {
            av.has_child_process == bv.has_child_process
                && av.remote_shell == bv.remote_shell
                && av.ssh_command == bv.ssh_command
        })
    })
}

/// Single-flight guard for the runtime probe. The 3s agent-status timer must
/// never queue overlapping `RuntimeProbeSnapshot::build` runs: a tick that fires
/// while a build is still in flight is dropped (coalesced), not queued.
///
/// Lives only on the GTK main thread — `Rc<Cell<bool>>` is intentionally
/// `!Send`. The returned [`ProbeGuard`] is held across the worker `await` inside
/// a `spawn_future_local` future, which does not require `Send`.
#[derive(Clone, Default)]
pub(crate) struct ProbeInFlight(Rc<Cell<bool>>);

impl ProbeInFlight {
    /// Begin a probe if none is in flight. Returns `None` (caller should skip
    /// this tick) when a build is already running; otherwise marks the flag and
    /// returns a guard that clears it on drop.
    pub(crate) fn try_begin(&self) -> Option<ProbeGuard> {
        if self.0.get() {
            return None;
        }
        self.0.set(true);
        Some(ProbeGuard(self.0.clone()))
    }
}

/// RAII guard that clears the [`ProbeInFlight`] flag when dropped, so a probe
/// can never wedge the flag permanently even if the future is dropped or the
/// worker errors mid-flight.
pub(crate) struct ProbeGuard(Rc<Cell<bool>>);

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

pub(crate) fn pane_process_states_from_cached_probe<S: RuntimeProbeSource + ?Sized>(
    _source: &S,
    cached: Option<&RuntimeProbeSnapshot>,
    _now_ms: u64,
    pane_pids: &[(u32, u32, i32)],
    _ttl_ms: u64,
) -> HashMap<PaneKey, PaneProcessState> {
    let Some(snapshot) = cached else {
        return HashMap::new();
    };
    pane_pids
        .iter()
        .filter_map(|(tab_id, pane_id, pid)| {
            let key = (*tab_id, *pane_id);
            (snapshot.pane_pids.get(&key) == Some(pid))
                .then(|| snapshot.pane_process_states.get(&key).cloned())
                .flatten()
                .map(|state| (key, state))
        })
        .collect()
}

/// Per-pane agent statuses from the latest completed probe. Snapshot builders
/// must never fall back to `/proc` reads on the GTK/socket thread; freshness is
/// reported separately in the runtime-probe truth payload.
pub(crate) fn pane_agents_from_cached_probe<S: RuntimeProbeSource + ?Sized>(
    _source: &S,
    cached: Option<&RuntimeProbeSnapshot>,
    _now_ms: u64,
    pane_pids: &[(u32, u32, i32)],
    _ttl_ms: u64,
) -> HashMap<PaneKey, AgentStatus> {
    let Some(snapshot) = cached else {
        return HashMap::new();
    };
    pane_pids
        .iter()
        .filter_map(|(tab_id, pane_id, pid)| {
            let key = (*tab_id, *pane_id);
            (snapshot.pane_pids.get(&key) == Some(pid))
                .then(|| snapshot.pane_agents.get(&key).cloned())
                .flatten()
                .map(|status| (key, status))
        })
        .collect()
}

/// All running agents in one tab, resolved per pane, plus the primary agent
/// used for the tab-level single-agent compatibility fields.
///
/// The primary is the focused pane's agent when the focused pane has one,
/// otherwise the first running agent in pane order, otherwise the tab-level
/// probe fallback. This keeps selection deterministic instead of
/// first-match-wins over an arbitrary pane iteration.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct TabAgentsView {
    pub agents: Vec<(u32, AgentStatus)>,
    pub primary: Option<(u32, AgentStatus)>,
}

pub(crate) fn resolve_tab_agents(
    snapshot: &RuntimeProbeSnapshot,
    tab_id: u32,
    local_pane_ids: &[u32],
    focused_pane_id: u32,
) -> TabAgentsView {
    let agents: Vec<(u32, AgentStatus)> = local_pane_ids
        .iter()
        .filter_map(|pane_id| {
            snapshot
                .pane_agents
                .get(&(tab_id, *pane_id))
                .filter(|status| status.running)
                .map(|status| (*pane_id, status.clone()))
        })
        .collect();
    let primary = agents
        .iter()
        .find(|(pane_id, _)| *pane_id == focused_pane_id)
        .or_else(|| agents.first())
        .cloned()
        .or_else(|| {
            snapshot
                .tab_agents
                .get(&tab_id)
                .filter(|status| status.running)
                .map(|status| (focused_pane_id, status.clone()))
        });
    TabAgentsView { agents, primary }
}

/// A single running agent in a tab, carrying a stable, human-facing label that
/// disambiguates multiple instances of the same agent kind within one tab.
///
/// The label is derived deterministically from pane order: the first `codex`
/// pane is `codex`, and a second becomes `codex #2`. A kind that appears only
/// once keeps its bare name so single-instance tabs read cleanly. Pane order is
/// the tiebreaker, so labels stay stable across scans as long as the pane set
/// does not change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentInstance {
    pub pane_id: u32,
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    /// Stable per-kind label, e.g. `codex #2`. Falls back to `agent (pane N)`
    /// when the agent kind is unknown.
    pub instance_label: String,
    /// 1-based index within this kind, in pane order. `1` for the only/first
    /// instance of a kind.
    pub kind_index: usize,
}

/// Assign stable, disambiguated labels to a tab's ordered running agents.
///
/// `agents` must already be in pane order (as produced by
/// [`resolve_tab_agents`]). The returned vector preserves that order. Each entry
/// gets a 1-based `kind_index` counting occurrences of the same agent name, and
/// an `instance_label` that only appends `#N` when the kind repeats.
pub(crate) fn assign_agent_instance_labels(agents: &[(u32, AgentStatus)]) -> Vec<AgentInstance> {
    use std::collections::HashMap;

    // First pass: total count per kind, so we know whether to disambiguate.
    let mut totals: HashMap<&str, usize> = HashMap::new();
    for (_, status) in agents {
        if let Some(name) = status.agent_name.as_deref() {
            *totals.entry(name).or_insert(0) += 1;
        }
    }

    // Second pass: running index per kind, in pane order.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    agents
        .iter()
        .map(|(pane_id, status)| {
            let name = status.agent_name.as_deref();
            let (kind_index, instance_label) = match name {
                Some(name) => {
                    let index = {
                        let counter = seen.entry(name).or_insert(0);
                        *counter += 1;
                        *counter
                    };
                    let total = totals.get(name).copied().unwrap_or(1);
                    let label = if total > 1 {
                        format!("{name} #{index}")
                    } else {
                        name.to_string()
                    };
                    (index, label)
                }
                // Unknown kind: never mergeable, so key it by pane for a stable label.
                None => (1, format!("agent (pane {pane_id})")),
            };
            AgentInstance {
                pane_id: *pane_id,
                agent_name: status.agent_name.clone(),
                session_id: status.session_id.clone(),
                instance_label,
                kind_index,
            }
        })
        .collect()
}

fn pane_pid_key(pane_pids: &[(u32, u32, i32)]) -> BTreeMap<PaneKey, i32> {
    pane_pids
        .iter()
        .map(|(tab_id, pane_id, pid)| ((*tab_id, *pane_id), *pid))
        .collect()
}

fn tab_pid_key(tab_pids: &[(u32, Vec<i32>)]) -> BTreeMap<u32, Vec<i32>> {
    tab_pids
        .iter()
        .map(|(tab_id, pids)| {
            let mut pids = pids.clone();
            pids.sort_unstable();
            pids.dedup();
            (*tab_id, pids)
        })
        .collect()
}

fn effective_probe_state(base: ProbeState, fresh: bool) -> ProbeState {
    if fresh || !matches!(base, ProbeState::Ok) {
        base
    } else {
        ProbeState::Stale
    }
}

#[derive(Clone, Debug, Default)]
struct ProcessFacts {
    // Process metadata is populated lazily. A periodic port projection needs
    // every descendant's children and socket fds, but agent/SSH detection only
    // needs names and command lines near the pane root, and cwd only for the
    // root. Eagerly reading every field turned one tree walk into four `/proc`
    // reads per descendant even when three of those values were never used.
    children: OnceCell<Vec<i32>>,
    comm: OnceCell<Option<String>>,
    cmdline: OnceCell<Option<Vec<String>>>,
    cwd: OnceCell<Option<String>>,
    socket_inodes: OnceCell<Vec<u64>>,
}

#[derive(Default)]
struct ProcessTreeCache {
    processes: HashMap<i32, ProcessFacts>,
    agent_statuses: HashMap<i32, AgentStatus>,
    process_error: Option<String>,
}

impl ProcessTreeCache {
    fn facts(&mut self, pid: i32) -> &ProcessFacts {
        self.processes.entry(pid).or_default()
    }

    fn validate_root<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) {
        match source.probe_process_root(pid) {
            Ok((children, comm)) => {
                let facts = self.facts(pid);
                let _ = facts.children.set(children);
                let _ = facts.comm.set(comm);
            }
            Err(error) => {
                self.process_error.get_or_insert(error);
            }
        }
    }

    fn children<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) -> Vec<i32> {
        self.facts(pid)
            .children
            .get_or_init(|| source.child_pids(pid))
            .clone()
    }

    fn process_tuple<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        pid: i32,
    ) -> ProcessFact {
        (pid, self.comm(source, pid), self.cmdline(source, pid))
    }

    fn comm<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) -> Option<String> {
        self.facts(pid)
            .comm
            .get_or_init(|| source.process_comm(pid))
            .clone()
    }

    fn cmdline<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        pid: i32,
    ) -> Option<Vec<String>> {
        self.facts(pid)
            .cmdline
            .get_or_init(|| source.process_cmdline(pid))
            .clone()
    }

    fn cwd<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) -> Option<String> {
        self.facts(pid)
            .cwd
            .get_or_init(|| source.process_cwd(pid))
            .clone()
    }

    fn is_ssh<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) -> bool {
        self.comm(source, pid)
            .as_deref()
            .is_some_and(agents::is_ssh_process)
    }

    fn socket_inodes<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, pid: i32) -> Vec<u64> {
        self.facts(pid)
            .socket_inodes
            .get_or_init(|| source.socket_inodes(pid))
            .clone()
    }

    fn collect_tree<S: RuntimeProbeSource + ?Sized>(&mut self, source: &S, root: i32) -> Vec<i32> {
        let mut tree = Vec::new();
        let mut stack = vec![root];
        let mut seen = HashSet::new();

        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            tree.push(pid);
            for child in self.children(source, pid) {
                stack.push(child);
            }
        }

        tree
    }

    fn collect_agent_tree<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        root: i32,
    ) -> Vec<i32> {
        let mut tree = Vec::new();
        let mut queue = VecDeque::from([root]);
        let mut seen = HashSet::new();

        while let Some(pid) = queue.pop_front() {
            if !seen.insert(pid) {
                continue;
            }
            tree.push(pid);
            if tree.len() >= AGENT_SCAN_MAX_PROCESSES {
                break;
            }
            queue.extend(self.children(source, pid));
        }

        tree
    }

    fn pane_process_state<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        pid: i32,
        now_ms: u64,
    ) -> PaneProcessState {
        let children = self.children(source, pid);
        let ssh_command = self.detect_ssh_command(source, pid, &children);

        PaneProcessState {
            // `/proc` can only describe children on this machine. For an SSH
            // pane this may describe the ssh client or its local wrapper, never
            // the remote foreground command. API serialization masks this
            // local-only observation to null when remote_shell is true or
            // authoritative remote tmux metadata identifies the boundary.
            has_child_process: !children.is_empty(),
            remote_shell: ssh_command.is_some(),
            ssh_command,
            updated_at_unix_ms: Some(now_ms),
        }
    }

    fn detect_ssh_command<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        pid: i32,
        direct_children: &[i32],
    ) -> Option<Vec<String>> {
        if self.is_ssh(source, pid) {
            return self.cmdline(source, pid);
        }

        for child_pid in direct_children {
            if self.is_ssh(source, *child_pid) {
                return self.cmdline(source, *child_pid);
            }
            for grandchild in self.children(source, *child_pid) {
                if self.is_ssh(source, grandchild) {
                    return self.cmdline(source, grandchild);
                }
            }
        }

        None
    }

    fn exact_agent_status_for_root<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        root: i32,
    ) -> Option<AgentStatus> {
        let tree = self.collect_agent_tree(source, root);
        if tree.len() >= AGENT_SCAN_MAX_PROCESSES {
            return None;
        }
        let cwd = self.cwd(source, root);
        let facts = tree
            .into_iter()
            .map(|pid| {
                (
                    pid,
                    source.verified_executable_name(pid),
                    self.cmdline(source, pid),
                )
            })
            .collect::<Vec<_>>();
        let exact = agents::detect_exact_agent_in_process_facts(cwd.as_deref(), &facts)?;
        // A native tool nested under an interpreted agent is not the pane
        // owner. Advisory evidence may veto a conflicting owner; it can never
        // supply the executable/session proof required above.
        (self.agent_status_for_root(source, root) == exact).then_some(exact)
    }

    fn agent_status_for_root<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        root: i32,
    ) -> AgentStatus {
        if let Some(status) = self.agent_statuses.get(&root) {
            return status.clone();
        }
        let cwd = self.cwd(source, root);
        // Agent launchers commonly add several wrapper layers (shell, secret
        // injector, TUI runner). Scan breadth-first so an owning agent appears
        // before its tool subprocesses, with a process-count bound for uncached
        // synchronous probes.
        let process_tree = self.collect_agent_tree(source, root);
        let candidates = process_tree
            .into_iter()
            .map(|pid| self.process_tuple(source, pid))
            .collect::<Vec<_>>();

        let status = agents::detect_agent_in_process_facts(cwd.as_deref(), &candidates);
        self.agent_statuses.insert(root, status.clone());
        status
    }

    fn tab_ports<S: RuntimeProbeSource + ?Sized>(
        &mut self,
        source: &S,
        tab_pids: &[(u32, Vec<i32>)],
        listen_table: &HashMap<u64, u16>,
    ) -> HashMap<u32, Vec<u16>> {
        if listen_table.is_empty() {
            return HashMap::new();
        }

        let mut results = HashMap::new();
        for (tab_id, pids) in tab_pids {
            let mut ports = Vec::new();
            for pid in pids {
                for proc_pid in self.collect_tree(source, *pid) {
                    for inode in self.socket_inodes(source, proc_pid) {
                        if let Some(port) = listen_table.get(&inode) {
                            ports.push(*port);
                        }
                    }
                }
            }
            ports.sort_unstable();
            ports.dedup();
            if !ports.is_empty() {
                results.insert(*tab_id, ports);
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct FakeProbeSource {
        children: HashMap<i32, Vec<i32>>,
        comm: HashMap<i32, String>,
        executables: HashMap<i32, String>,
        cmdline: HashMap<i32, Vec<String>>,
        cwd: HashMap<i32, String>,
        socket_inodes: HashMap<i32, Vec<u64>>,
        listen_table: HashMap<u64, u16>,
        listen_partial_error: Option<String>,
        listen_error: Option<String>,
        process_error: Option<String>,
        child_reads: Cell<usize>,
        comm_reads: Cell<usize>,
        cmdline_reads: Cell<usize>,
        cwd_reads: Cell<usize>,
        socket_reads: Cell<usize>,
        listen_reads: Cell<usize>,
    }

    impl FakeProbeSource {
        fn total_reads(&self) -> usize {
            self.child_reads.get()
                + self.comm_reads.get()
                + self.cmdline_reads.get()
                + self.cwd_reads.get()
                + self.socket_reads.get()
                + self.listen_reads.get()
        }

        fn with_projection_fixture() -> Self {
            let mut source = Self::default();
            source.children.insert(10, vec![11, 13]);
            source.children.insert(11, vec![12]);
            source.comm.insert(10, "bash".to_string());
            source.comm.insert(11, "env".to_string());
            source.comm.insert(12, "pi".to_string());
            source.comm.insert(13, "ssh".to_string());
            source.cmdline.insert(
                12,
                vec![
                    "pi".to_string(),
                    "--session".to_string(),
                    "sess-1".to_string(),
                ],
            );
            source
                .cmdline
                .insert(13, vec!["ssh".to_string(), "dev.ts".to_string()]);
            source.cwd.insert(10, "/workspace".to_string());
            source.socket_inodes.insert(12, vec![9001]);
            source.listen_table.insert(9001, 8080);
            source
        }
    }

    impl RuntimeProbeSource for FakeProbeSource {
        fn verified_executable_name(&self, pid: i32) -> Option<String> {
            self.executables.get(&pid).cloned()
        }

        fn probe_process_root(&self, pid: i32) -> Result<(Vec<i32>, Option<String>), String> {
            if let Some(error) = &self.process_error {
                return Err(error.clone());
            }
            Ok((self.child_pids(pid), self.process_comm(pid)))
        }

        fn child_pids(&self, pid: i32) -> Vec<i32> {
            self.child_reads.set(self.child_reads.get() + 1);
            self.children.get(&pid).cloned().unwrap_or_default()
        }

        fn process_comm(&self, pid: i32) -> Option<String> {
            self.comm_reads.set(self.comm_reads.get() + 1);
            self.comm.get(&pid).cloned()
        }

        fn process_cmdline(&self, pid: i32) -> Option<Vec<String>> {
            self.cmdline_reads.set(self.cmdline_reads.get() + 1);
            self.cmdline.get(&pid).cloned()
        }

        fn process_cwd(&self, pid: i32) -> Option<String> {
            self.cwd_reads.set(self.cwd_reads.get() + 1);
            self.cwd.get(&pid).cloned()
        }

        fn socket_inodes(&self, pid: i32) -> Vec<u64> {
            self.socket_reads.set(self.socket_reads.get() + 1);
            self.socket_inodes.get(&pid).cloned().unwrap_or_default()
        }

        fn listen_table(&self) -> ListenTableProbe {
            self.listen_reads.set(self.listen_reads.get() + 1);
            match (&self.listen_error, &self.listen_partial_error) {
                (Some(error), _) => ListenTableProbe::Failed {
                    error: error.clone(),
                },
                (None, Some(error)) => ListenTableProbe::Partial {
                    table: self.listen_table.clone(),
                    error: error.clone(),
                },
                (None, None) => ListenTableProbe::Complete(self.listen_table.clone()),
            }
        }
    }

    /// Fixture for a tab with three agent panes: two claude sessions and one
    /// codex. Pane roots are shells with the agent as a direct child, except
    /// pane 9 where the agent process IS the pane root.
    fn multi_agent_fixture() -> FakeProbeSource {
        let mut source = FakeProbeSource::default();
        // pane 7 root 20: bash -> claude --session sess-a
        source.children.insert(20, vec![21]);
        source.comm.insert(20, "bash".to_string());
        source.comm.insert(21, "claude".to_string());
        source.cmdline.insert(
            21,
            vec![
                "claude".to_string(),
                "--session".to_string(),
                "sess-a".to_string(),
            ],
        );
        // pane 8 root 30: bash -> claude --session sess-b
        source.children.insert(30, vec![31]);
        source.comm.insert(30, "bash".to_string());
        source.comm.insert(31, "claude".to_string());
        source.cmdline.insert(
            31,
            vec![
                "claude".to_string(),
                "--session".to_string(),
                "sess-b".to_string(),
            ],
        );
        // pane 9 root 40: codex is the pane root itself (no shell wrapper)
        source.comm.insert(40, "codex".to_string());
        source.cmdline.insert(40, vec!["codex".to_string()]);
        source
    }

    fn multi_agent_snapshot(source: &FakeProbeSource) -> RuntimeProbeSnapshot {
        RuntimeProbeSnapshot::build(
            source,
            1_000,
            &[(1, vec![20, 30, 40])],
            &[(1, 7, 20), (1, 8, 30), (1, 9, 40)],
        )
    }

    #[test]
    fn exact_attach_local_tmux_root_requires_fresh_positive_local_evidence() {
        let mut backing = crate::pane::TmuxBacking {
            session_name: "fixture".into(),
            target: crate::tmux::TmuxTarget::Local,
            pane_info: crate::probe::ProbeSnapshot {
                state: ProbeState::Ok,
                value: Some(crate::tmux::TmuxPaneInfo {
                    current_command: "codex".into(),
                    cwd: "/synthetic".into(),
                    pid: 77,
                    width: 80,
                    height: 24,
                }),
                observed_at_unix_ms: Some(1_000),
                checked_at_unix_ms: Some(1_000),
                error: None,
            },
        };
        assert_eq!(
            local_pane_process_root(Some(10), Some(&backing), 1_001),
            Some(77)
        );
        // A healthy five-second metadata cycle must not switch to the tmux
        // client after the independent three-second process-cache TTL.
        for now_ms in [4_001, 5_999, 6_000] {
            let root = local_pane_process_root(Some(10), Some(&backing), now_ms).unwrap();
            assert_eq!(root, 77);
            let snapshot = RuntimeProbeSnapshot::build(
                &FakeProbeSource::default(),
                now_ms - 1,
                &[(1, vec![10])],
                &[(1, 7, 77)],
            );
            assert!(snapshot.is_fresh_for(&[(1, vec![10])], &[(1, 7, root)], now_ms, PROBE_TTL_MS));
        }
        assert_eq!(
            local_pane_process_root(Some(10), Some(&backing), 11_001),
            Some(10)
        );
        assert_eq!(
            local_pane_process_root(Some(10), Some(&backing), 999),
            Some(10)
        );
        for state in [ProbeState::Stale, ProbeState::Error, ProbeState::Unknown] {
            backing.pane_info.state = state;
            assert_eq!(
                local_pane_process_root(Some(10), Some(&backing), 1_001),
                Some(10)
            );
            assert_eq!(fresh_local_tmux_pid(&backing, 1_001), None);
        }
        backing.pane_info.state = ProbeState::Ok;
        assert_eq!(fresh_local_tmux_pid(&backing, 11_000), Some(77));
        assert_eq!(fresh_local_tmux_pid(&backing, 11_001), None);
        assert_eq!(fresh_local_tmux_pid(&backing, 999), None);
        assert_eq!(local_pane_process_root(None, Some(&backing), 1_001), None);
        backing.pane_info.observed_at_unix_ms = None;
        assert_eq!(fresh_local_tmux_pid(&backing, 1_001), None);
        backing.pane_info.observed_at_unix_ms = Some(1_000);
        backing.target = crate::tmux::TmuxTarget::Remote {
            ssh_target: "fixture.ts".into(),
        };
        assert_eq!(
            local_pane_process_root(Some(10), Some(&backing), 1_001),
            Some(10)
        );
        backing.target = crate::tmux::TmuxTarget::Local;
        backing.pane_info.value.as_mut().unwrap().pid = 0;
        assert_eq!(
            local_pane_process_root(Some(10), Some(&backing), 1_001),
            Some(10)
        );
        let source = FakeProbeSource::default();
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 77)]);
        assert!(!snapshot.is_fresh_for(&[(1, vec![10])], &[(1, 7, 88)], 1_001, PROBE_TTL_MS));
    }

    #[test]
    fn exact_attach_identity_rejects_broad_titles_and_wrapper_arguments() {
        for (executable, args) in [
            ("/synthetic/notcodex", argv(&["notcodex", "resume", "same"])),
            (
                "/synthetic/echo",
                argv(&["echo", "codex", "resume", "same"]),
            ),
            (
                "/synthetic/runner",
                argv(&["runner", "--", "codex", "resume", "same"]),
            ),
        ] {
            let mut source = FakeProbeSource::default();
            source.comm.insert(10, "codex".into());
            source.executables.insert(10, executable.into());
            source.cmdline.insert(10, args);
            let snapshot =
                RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);
            assert!(
                snapshot.pane_agents[&(1, 7)].running,
                "advisory detection remains available"
            );
            assert!(snapshot.pane_exact_agents.is_empty());
        }
    }

    #[test]
    fn exact_attach_identity_requires_one_explicit_provider_owner_and_fresh_probe() {
        let mut source = FakeProbeSource::default();
        source.executables.insert(10, "/synthetic/codex".into());
        source
            .cmdline
            .insert(10, argv(&["codex", "resume", "exact-id"]));
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);
        assert_eq!(
            snapshot.pane_exact_agents[&(1, 7)].session_id.as_deref(),
            Some("exact-id")
        );
        let failed = RuntimeProbeSnapshot::worker_failure(
            1_001,
            &[(1, vec![10])],
            &[(1, 7, 10)],
            RuntimeProbeWorkerFailure::Panicked,
        )
        .reconcile(Some(&snapshot));
        assert!(failed.pane_exact_agents.is_empty());
        source.children.insert(10, vec![11]);
        source.executables.insert(11, "/synthetic/claude".into());
        source
            .cmdline
            .insert(11, argv(&["claude", "--resume", "second-id"]));
        let ambiguous =
            RuntimeProbeSnapshot::build(&source, 1_002, &[(1, vec![10])], &[(1, 7, 10)]);
        assert!(ambiguous.pane_exact_agents.is_empty());
        source.children.clear();
        for args in [
            argv(&["codex", "exec", "resume", "payload"]),
            argv(&["codex", "resume", "id", "--last"]),
            argv(&["codex", "resume", "id", "--session", "other"]),
            argv(&["codex", "--", "resume", "payload"]),
        ] {
            source.cmdline.insert(10, args);
            assert!(
                RuntimeProbeSnapshot::build(&source, 1_003, &[(1, vec![10])], &[(1, 7, 10)])
                    .pane_exact_agents
                    .is_empty()
            );
        }
    }

    #[test]
    fn exact_attach_identity_never_promotes_a_tool_beneath_an_interpreted_owner() {
        let mut source = FakeProbeSource::default();
        source.children.insert(10, vec![11]);
        source.executables.insert(10, "/synthetic/node".into());
        source.executables.insert(11, "/synthetic/codex".into());
        source.comm.insert(10, "node".into());
        source.comm.insert(11, "codex".into());
        source
            .cmdline
            .insert(10, argv(&["node", "/synthetic/pi", "--session", "owner"]));
        source
            .cmdline
            .insert(11, argv(&["codex", "resume", "tool"]));
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);
        assert_eq!(
            snapshot.pane_agents[&(1, 7)].agent_name.as_deref(),
            Some("pi")
        );
        assert!(snapshot.pane_exact_agents.is_empty());
    }

    #[test]
    fn pi_wrapper_tree_is_not_misclassified_as_codex_provider() {
        let mut source = FakeProbeSource::default();
        source.children.insert(50, vec![51]);
        source.children.insert(51, vec![52]);
        source.children.insert(52, vec![53]);
        source.children.insert(53, vec![54]);
        source.children.insert(54, vec![55]);
        source.comm.insert(50, "zsh".to_string());
        source.comm.insert(51, "bash".to_string());
        source.comm.insert(52, "infisical".to_string());
        source.comm.insert(53, "bash".to_string());
        source.comm.insert(54, "pi".to_string());
        source.comm.insert(55, "codex".to_string());
        source.cmdline.insert(
            52,
            vec![
                "infisical".to_string(),
                "run".to_string(),
                "--".to_string(),
                "bash".to_string(),
                "-lc".to_string(),
                "export CODEX_GITHUB_PERSONAL_ACCESS_TOKEN; exec $PI_BIN".to_string(),
            ],
        );
        source.cmdline.insert(53, vec!["bash".to_string()]);
        source.cmdline.insert(
            54,
            vec![
                "pi".to_string(),
                "--session".to_string(),
                "pi-session".to_string(),
            ],
        );
        source.cmdline.insert(
            55,
            vec![
                "codex".to_string(),
                "app-server".to_string(),
                "--stdio".to_string(),
            ],
        );

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![50])], &[(1, 7, 50)]);

        let pane_agent = &snapshot.pane_agents[&(1, 7)];
        assert!(pane_agent.running);
        assert_eq!(pane_agent.agent_name.as_deref(), Some("pi"));
        assert_eq!(pane_agent.session_id.as_deref(), Some("pi-session"));
        assert_eq!(snapshot.tab_agents[&1], *pane_agent);
    }

    #[test]
    fn launcher_agent_identity_outranks_pi_tool_subprocess() {
        let mut source = FakeProbeSource::default();
        source.children.insert(60, vec![61]);
        source.children.insert(61, vec![62]);
        source.comm.insert(60, "zsh".to_string());
        source.comm.insert(61, "node".to_string());
        source.comm.insert(62, "pi".to_string());
        source.cmdline.insert(
            61,
            vec![
                "node".to_string(),
                "/usr/bin/npx".to_string(),
                "--yes".to_string(),
                "--package".to_string(),
                "opencode-ai".to_string(),
                "--".to_string(),
                "opencode".to_string(),
            ],
        );
        source.cmdline.insert(62, vec!["pi".to_string()]);

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![60])], &[(1, 7, 60)]);

        let pane_agent = &snapshot.pane_agents[&(1, 7)];
        assert_eq!(pane_agent.agent_name.as_deref(), Some("opencode"));
    }

    /// The `bash -lc` program the real `pi-run` launcher hands to
    /// `infisical run`, reproduced (trimmed) from a live process tree. It names
    /// Codex, Copilot and Kimi while the pane is owned by Pi.
    const PI_RUN_WRAPPER_SCRIPT: &str = r#"
    # Infisical injects Pi provider secrets here (for example KIMI_API_KEY,
    # MINIMAX_API_KEY, and LITELLM_API_KEY). GitHub auth remains available to gh
    # and Codex, but Copilot credentials are deliberately withheld from Pi.
    export PATH="$PI_RUN_PATH"

    unset COPILOT_GITHUB_TOKEN

    codex_token="${CODEX_GITHUB_PERSONAL_ACCESS_TOKEN:-${GITHUB_TOKEN:-}}"
    if [ -n "$codex_token" ]; then
        export CODEX_GITHUB_PERSONAL_ACCESS_TOKEN="$codex_token"
    fi

    exec "$PI_BIN" "$@"
"#;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// `zsh` -> `bash pi-run` -> `infisical run -- bash -lc <script>` -> `pi`,
    /// rooted at `root` with pids allocated contiguously from it.
    fn pi_run_launcher_chain(source: &mut FakeProbeSource, root: i32, pi_session: Option<&str>) {
        source.children.insert(root, vec![root + 1]);
        source.children.insert(root + 1, vec![root + 2]);
        source.comm.insert(root, "zsh".to_string());
        source.comm.insert(root + 1, "bash".to_string());
        source.comm.insert(root + 2, "infisical".to_string());
        source.cmdline.insert(root, argv(&["-zsh"]));
        source
            .cmdline
            .insert(root + 1, argv(&["bash", "/tmp/user/.local/bin/pi-run"]));
        source.cmdline.insert(
            root + 2,
            argv(&[
                "infisical",
                "run",
                "--projectId=00000000-0000-0000-0000-000000000000",
                "--env=dev",
                "--path=/",
                "--",
                "bash",
                "-lc",
                PI_RUN_WRAPPER_SCRIPT,
            ]),
        );

        let Some(session) = pi_session else {
            return;
        };
        source.children.insert(root + 2, vec![root + 3]);
        source.comm.insert(root + 3, "pi".to_string());
        source.cmdline.insert(
            root + 3,
            argv(&[
                "/tmp/user/.local/share/mise/installs/pi/latest/pi/pi",
                "--session",
                session,
            ]),
        );
    }

    #[test]
    fn real_pi_run_infisical_tree_reports_pi_not_codex() {
        let mut source = FakeProbeSource::default();
        pi_run_launcher_chain(&mut source, 70, Some("pi-session-a"));

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![70])], &[(1, 7, 70)]);

        let pane_agent = &snapshot.pane_agents[&(1, 7)];
        assert!(pane_agent.running);
        assert_eq!(pane_agent.agent_name.as_deref(), Some("pi"));
        assert_eq!(pane_agent.session_id.as_deref(), Some("pi-session-a"));
    }

    #[test]
    fn pi_run_launcher_without_pi_yet_leaves_the_pane_unlabelled() {
        let mut source = FakeProbeSource::default();
        pi_run_launcher_chain(&mut source, 80, None);

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![80])], &[(1, 7, 80)]);

        let pane_agent = &snapshot.pane_agents[&(1, 7)];
        assert_eq!(
            pane_agent.agent_name, None,
            "the wrapper's Codex mentions must not label a pane whose agent has \
             not started yet"
        );
        assert!(!pane_agent.running);
    }

    #[test]
    fn two_pi_panes_sharing_a_cwd_stay_separately_identified() {
        let mut source = FakeProbeSource::default();
        pi_run_launcher_chain(&mut source, 90, Some("pi-session-left"));
        pi_run_launcher_chain(&mut source, 110, Some("pi-session-right"));
        source.cwd.insert(90, "/work/shared".to_string());
        source.cwd.insert(110, "/work/shared".to_string());

        let snapshot = RuntimeProbeSnapshot::build(
            &source,
            1_000,
            &[(1, vec![90, 110])],
            &[(1, 7, 90), (1, 8, 110)],
        );

        let left = &snapshot.pane_agents[&(1, 7)];
        let right = &snapshot.pane_agents[&(1, 8)];
        assert_eq!(left.agent_name.as_deref(), Some("pi"));
        assert_eq!(right.agent_name.as_deref(), Some("pi"));
        assert_eq!(left.session_id.as_deref(), Some("pi-session-left"));
        assert_eq!(right.session_id.as_deref(), Some("pi-session-right"));
    }

    #[test]
    fn agent_detected_when_agent_is_pane_root() {
        let source = multi_agent_fixture();
        let snapshot = multi_agent_snapshot(&source);

        let pane_agent = &snapshot.pane_agents[&(1, 9)];
        assert!(pane_agent.running);
        assert_eq!(pane_agent.agent_name.as_deref(), Some("codex"));
    }

    #[test]
    fn resolve_tab_agents_keeps_every_running_pane_agent() {
        let source = multi_agent_fixture();
        let snapshot = multi_agent_snapshot(&source);

        let view = resolve_tab_agents(&snapshot, 1, &[7, 8, 9], 9);

        let names: Vec<(u32, Option<&str>, Option<&str>)> = view
            .agents
            .iter()
            .map(|(pane_id, status)| {
                (
                    *pane_id,
                    status.agent_name.as_deref(),
                    status.session_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            names,
            vec![
                (7, Some("claude"), Some("sess-a")),
                (8, Some("claude"), Some("sess-b")),
                (9, Some("codex"), None),
            ]
        );
    }

    #[test]
    fn resolve_tab_agents_prefers_focused_pane_for_primary() {
        let source = multi_agent_fixture();
        let snapshot = multi_agent_snapshot(&source);

        let focused = resolve_tab_agents(&snapshot, 1, &[7, 8, 9], 9);
        assert_eq!(
            focused
                .primary
                .as_ref()
                .map(|(pane_id, status)| (*pane_id, status.agent_name.as_deref())),
            Some((9, Some("codex")))
        );

        // Focused pane without an agent: fall back to first agent in pane order.
        let unfocused = resolve_tab_agents(&snapshot, 1, &[7, 8, 9], 99);
        assert_eq!(
            unfocused
                .primary
                .as_ref()
                .map(|(pane_id, status)| (*pane_id, status.session_id.as_deref())),
            Some((7, Some("sess-a")))
        );
    }

    #[test]
    fn assign_agent_instance_labels_disambiguates_same_kind() {
        // Two codex + one claude, in pane order 4, 5, 6.
        let agents = vec![
            (
                4,
                AgentStatus {
                    agent_name: Some("codex".into()),
                    session_id: Some("sess-c1".into()),
                    running: true,
                },
            ),
            (
                5,
                AgentStatus {
                    agent_name: Some("codex".into()),
                    session_id: Some("sess-c2".into()),
                    running: true,
                },
            ),
            (
                6,
                AgentStatus {
                    agent_name: Some("claude".into()),
                    session_id: Some("sess-cl".into()),
                    running: true,
                },
            ),
        ];

        let instances = assign_agent_instance_labels(&agents);
        let labels: Vec<(u32, &str, usize)> = instances
            .iter()
            .map(|i| (i.pane_id, i.instance_label.as_str(), i.kind_index))
            .collect();
        assert_eq!(
            labels,
            vec![
                (4, "codex #1", 1),
                (5, "codex #2", 2),
                // Single-instance kind keeps its bare name (no #1 suffix).
                (6, "claude", 1),
            ]
        );
    }

    #[test]
    fn assign_agent_instance_labels_labels_unknown_kind_by_pane() {
        let agents = vec![(
            9,
            AgentStatus {
                agent_name: None,
                session_id: None,
                running: true,
            },
        )];
        let instances = assign_agent_instance_labels(&agents);
        assert_eq!(instances[0].instance_label, "agent (pane 9)");
    }

    #[test]
    fn resolve_tab_agents_falls_back_to_tab_probe() {
        let source = FakeProbeSource::with_projection_fixture();
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[]);

        let view = resolve_tab_agents(&snapshot, 1, &[], 5);
        assert!(view.agents.is_empty());
        assert_eq!(
            view.primary
                .as_ref()
                .map(|(pane_id, status)| (*pane_id, status.agent_name.as_deref())),
            Some((5, Some("pi")))
        );
    }

    #[test]
    fn runtime_probe_cache_reuse_avoids_source_reads() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);
        let reads_after_build = source.total_reads();

        let states = pane_process_states_from_cached_probe(
            &source,
            Some(&snapshot),
            2_000,
            &pane_pids,
            PROBE_TTL_MS,
        );

        assert_eq!(source.total_reads(), reads_after_build);
        assert_eq!(
            states
                .get(&(1, 7))
                .and_then(|state| state.updated_at_unix_ms),
            Some(1_000)
        );
    }

    #[test]
    fn runtime_probe_failure_expired_cache_never_reprobes_on_snapshot_thread() {
        let source = FakeProbeSource::with_projection_fixture();
        let pane_pids = vec![(1, 7, 10)];
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &pane_pids);
        let reads_after_build = source.total_reads();

        let states = pane_process_states_from_cached_probe(
            &source,
            Some(&snapshot),
            1_000 + PROBE_TTL_MS + 1,
            &pane_pids,
            PROBE_TTL_MS,
        );

        assert_eq!(source.total_reads(), reads_after_build);
        assert!(states[&(1, 7)].has_child_process);
    }

    #[test]
    fn runtime_probe_reads_only_metadata_used_by_each_projection() {
        let mut source = FakeProbeSource::default();
        for pid in 10..22 {
            if pid < 21 {
                source.children.insert(pid, vec![pid + 1]);
            }
            source.comm.insert(pid, format!("worker-{pid}"));
            source.cmdline.insert(pid, vec![format!("worker-{pid}")]);
            source.cwd.insert(pid, format!("/workspace/{pid}"));
            source.socket_inodes.insert(pid, vec![pid as u64]);
            source.listen_table.insert(pid as u64, 3_000 + pid as u16);
        }

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);

        assert_eq!(snapshot.tab_ports[&1].len(), 12);
        assert_eq!(source.child_reads.get(), 12, "the tree is walked once");
        assert_eq!(
            source.comm_reads.get(),
            12,
            "bounded agent classification inspects the process tree"
        );
        assert_eq!(
            source.cmdline_reads.get(),
            12,
            "bounded agent classification inspects the process tree"
        );
        assert_eq!(
            source.cwd_reads.get(),
            1,
            "only the pane root contributes agent-signature overrides"
        );
        assert_eq!(
            source.socket_reads.get(),
            12,
            "port projection inspects sockets across the whole tree"
        );
        assert_eq!(source.listen_reads.get(), 1);
    }

    #[test]
    fn runtime_probe_cache_invalidates_when_pane_pid_set_changes() {
        let source = FakeProbeSource::with_projection_fixture();
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);

        assert!(!snapshot.is_fresh_for(&[(1, vec![10])], &[(1, 7, 99)], 2_000, PROBE_TTL_MS));
    }

    #[test]
    fn runtime_probe_cache_invalidates_when_tab_pid_set_changes() {
        let source = FakeProbeSource::with_projection_fixture();
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);

        assert!(!snapshot.is_fresh_for(&[(1, vec![99])], &[(1, 7, 10)], 2_000, PROBE_TTL_MS));
    }

    #[test]
    fn runtime_probe_cache_invalidates_when_ttl_expires() {
        let source = FakeProbeSource::with_projection_fixture();
        let pane_pids = vec![(1, 7, 10)];
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &pane_pids);

        let tab_pids = vec![(1, vec![10])];
        assert!(!snapshot.is_fresh_for(
            &tab_pids,
            &pane_pids,
            1_000 + PROBE_TTL_MS + 1,
            PROBE_TTL_MS
        ));
        assert_eq!(
            snapshot.process_probe_state_for(
                &tab_pids,
                &pane_pids,
                1_000 + PROBE_TTL_MS + 1,
                PROBE_TTL_MS
            ),
            ProbeState::Stale
        );
    }

    #[test]
    fn runtime_probe_listen_table_failure_preserves_process_projection() {
        let mut source = FakeProbeSource::with_projection_fixture();
        source.listen_error = Some("listen table failed".to_string());

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);

        assert_eq!(snapshot.process_probe, ProbeState::Ok);
        assert_eq!(snapshot.ports_probe, ProbeState::Error);
        assert_eq!(snapshot.ports_error.as_deref(), Some("listen table failed"));
        assert!(snapshot.tab_ports.is_empty());
        assert!(snapshot.pane_process_states[&(1, 7)].has_child_process);
        assert!(snapshot.pane_agents[&(1, 7)].running);
    }

    #[test]
    fn runtime_probe_failure_process_stale_ports_fresh_preserves_last_good() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let mut failed_source = FakeProbeSource::with_projection_fixture();
        failed_source.process_error = Some("proc source unreadable".to_string());
        let failed = RuntimeProbeSnapshot::build(&failed_source, 2_000, &tab_pids, &pane_pids);
        let reconciled = failed.reconcile(Some(&healthy));

        assert_eq!(reconciled.process_probe, ProbeState::Stale);
        assert_eq!(reconciled.ports_probe, ProbeState::Ok);
        assert_eq!(
            reconciled.process_error.as_deref(),
            Some("proc source unreadable")
        );
        assert_eq!(
            reconciled.pane_agents[&(1, 7)].agent_name.as_deref(),
            Some("pi")
        );
        assert!(reconciled.pane_process_states[&(1, 7)].has_child_process);
        assert_eq!(reconciled.tab_ports[&1], vec![8080]);
        assert_eq!(reconciled.process_observed_at_unix_ms, Some(1_000));
        assert_eq!(reconciled.ports_observed_at_unix_ms, Some(2_000));
    }

    #[test]
    fn runtime_probe_failure_ports_stale_process_fresh_preserves_last_good() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let mut failed_source = FakeProbeSource::with_projection_fixture();
        failed_source.listen_error = Some("listen table unreadable".to_string());
        let failed = RuntimeProbeSnapshot::build(&failed_source, 2_000, &tab_pids, &pane_pids);
        let reconciled = failed.reconcile(Some(&healthy));

        assert_eq!(reconciled.process_probe, ProbeState::Ok);
        assert_eq!(reconciled.ports_probe, ProbeState::Stale);
        assert_eq!(
            reconciled.ports_error.as_deref(),
            Some("listen table unreadable")
        );
        assert!(reconciled.pane_agents[&(1, 7)].running);
        assert_eq!(reconciled.tab_ports[&1], vec![8080]);
        assert_eq!(reconciled.process_observed_at_unix_ms, Some(2_000));
        assert_eq!(reconciled.ports_observed_at_unix_ms, Some(1_000));
    }

    #[test]
    fn runtime_probe_partial_port_tables_merge_positive_data_without_erasing_last_good() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let mut partial_source = FakeProbeSource::with_projection_fixture();
        partial_source.socket_inodes.insert(12, vec![9001, 9002]);
        partial_source.listen_table.clear();
        partial_source.listen_table.insert(9002, 9090);
        partial_source.listen_partial_error = Some(format!(
            "3 of 4 /proc/net tables unreadable: tcp6 denied; udp failed; udp6 denied {}",
            "x".repeat(PROBE_REASON_MAX_CHARS * 2)
        ));

        let partial = RuntimeProbeSnapshot::build(&partial_source, 2_000, &tab_pids, &pane_pids);
        assert_eq!(partial.ports_probe, ProbeState::Error);
        assert_eq!(partial.tab_ports[&1], vec![9090]);
        assert_eq!(partial.ports_observed_at_unix_ms, None);

        let reconciled = partial.reconcile(Some(&healthy));
        let truth = runtime_probe_truth(
            Some(&reconciled),
            &tab_pids,
            &pane_pids,
            2_000,
            PROBE_TTL_MS,
        );

        assert_eq!(reconciled.ports_probe, ProbeState::Stale);
        assert_eq!(reconciled.tab_ports[&1], vec![8080, 9090]);
        assert_eq!(reconciled.ports_observed_at_unix_ms, Some(1_000));
        assert_eq!(truth.state, RuntimeProbeState::Partial);
        assert_eq!(truth.process_state, ProbeState::Ok);
        assert_eq!(truth.ports_state, ProbeState::Stale);
        assert!(truth
            .ports_error
            .as_deref()
            .is_some_and(|error| error.contains("3 of 4")));
        assert_eq!(
            truth
                .ports_error
                .as_deref()
                .expect("partial error")
                .chars()
                .count(),
            PROBE_REASON_MAX_CHARS
        );
        assert!(truth
            .ports_error
            .as_deref()
            .expect("partial error")
            .ends_with('…'));
    }

    #[test]
    fn runtime_probe_all_port_tables_failed_retains_only_last_good() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let mut failed_source = FakeProbeSource::with_projection_fixture();
        failed_source.listen_error = Some("4 of 4 /proc/net tables unreadable".to_string());
        let failed = RuntimeProbeSnapshot::build(&failed_source, 2_000, &tab_pids, &pane_pids);
        assert_eq!(failed.ports_probe, ProbeState::Error);
        assert!(failed.tab_ports.is_empty());
        let failed_truth =
            runtime_probe_truth(Some(&failed), &tab_pids, &pane_pids, 2_000, PROBE_TTL_MS);
        assert_eq!(failed_truth.state, RuntimeProbeState::Partial);
        assert_eq!(failed_truth.process_state, ProbeState::Ok);
        assert_eq!(failed_truth.ports_state, ProbeState::Error);

        let reconciled = failed.reconcile(Some(&healthy));
        assert_eq!(reconciled.ports_probe, ProbeState::Stale);
        assert_eq!(reconciled.tab_ports[&1], vec![8080]);
        assert_eq!(reconciled.ports_observed_at_unix_ms, Some(1_000));
    }

    #[test]
    fn runtime_probe_complete_port_tables_recover_from_partial_without_stale_union() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let mut partial_source = FakeProbeSource::with_projection_fixture();
        partial_source.listen_partial_error = Some("tcp6 denied".to_string());
        let partial = RuntimeProbeSnapshot::build(&partial_source, 2_000, &tab_pids, &pane_pids)
            .reconcile(Some(&healthy));
        assert_eq!(partial.ports_probe, ProbeState::Stale);

        let mut recovered_source = FakeProbeSource::with_projection_fixture();
        recovered_source.socket_inodes.insert(12, vec![9002]);
        recovered_source.listen_table.clear();
        recovered_source.listen_table.insert(9002, 9090);
        let recovered =
            RuntimeProbeSnapshot::build(&recovered_source, 3_000, &tab_pids, &pane_pids)
                .reconcile(Some(&partial));

        assert_eq!(recovered.ports_probe, ProbeState::Ok);
        assert_eq!(recovered.tab_ports[&1], vec![9090]);
        assert_eq!(recovered.ports_observed_at_unix_ms, Some(3_000));
        assert_eq!(recovered.ports_error, None);
    }

    #[test]
    fn runtime_probe_failure_startup_without_process_roots_is_absent() {
        let truth = runtime_probe_truth(None, &[], &[], 1_000, PROBE_TTL_MS);

        assert_eq!(truth.state, RuntimeProbeState::Absent);
        assert_eq!(truth.process_state, ProbeState::Unknown);
        assert_eq!(truth.ports_state, ProbeState::Unknown);
        assert_eq!(truth.process_age_ms, None);
        assert_eq!(truth.ports_age_ms, None);
    }

    #[test]
    fn runtime_probe_failure_ttl_reports_stale_with_last_known_age() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);

        let truth = runtime_probe_truth(
            Some(&snapshot),
            &tab_pids,
            &pane_pids,
            1_000 + PROBE_TTL_MS + 1,
            PROBE_TTL_MS,
        );

        assert_eq!(truth.state, RuntimeProbeState::Stale);
        assert_eq!(truth.process_state, ProbeState::Stale);
        assert_eq!(truth.ports_state, ProbeState::Stale);
        assert_eq!(truth.process_age_ms, Some(PROBE_TTL_MS + 1));
        assert_eq!(truth.ports_age_ms, Some(PROBE_TTL_MS + 1));
    }

    #[test]
    fn runtime_probe_failure_worker_panic_and_cancel_are_recorded_truth() {
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];

        for (failure, expected) in [
            (
                RuntimeProbeWorkerFailure::Panicked,
                "runtime probe worker panicked",
            ),
            (
                RuntimeProbeWorkerFailure::Cancelled,
                "runtime probe worker cancelled",
            ),
        ] {
            let snapshot =
                RuntimeProbeSnapshot::worker_failure(2_000, &tab_pids, &pane_pids, failure);
            let truth =
                runtime_probe_truth(Some(&snapshot), &tab_pids, &pane_pids, 2_000, PROBE_TTL_MS);

            assert_eq!(truth.state, RuntimeProbeState::Failed);
            assert_eq!(truth.process_error.as_deref(), Some(expected));
            assert_eq!(truth.ports_error.as_deref(), Some(expected));
        }
    }

    #[test]
    fn runtime_probe_failure_recovers_after_stale_last_known_good() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];
        let healthy = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);
        let failed = RuntimeProbeSnapshot::worker_failure(
            2_000,
            &tab_pids,
            &pane_pids,
            RuntimeProbeWorkerFailure::Panicked,
        )
        .reconcile(Some(&healthy));
        assert_eq!(failed.process_probe, ProbeState::Stale);
        assert_eq!(failed.ports_probe, ProbeState::Stale);

        let recovered = RuntimeProbeSnapshot::build(&source, 3_000, &tab_pids, &pane_pids)
            .reconcile(Some(&failed));
        let truth =
            runtime_probe_truth(Some(&recovered), &tab_pids, &pane_pids, 3_000, PROBE_TTL_MS);

        assert_eq!(truth.state, RuntimeProbeState::Ok);
        assert_eq!(truth.process_error, None);
        assert_eq!(truth.ports_error, None);
        assert_eq!(truth.process_age_ms, Some(0));
        assert_eq!(truth.ports_age_ms, Some(0));
    }

    #[test]
    fn runtime_probe_failure_reason_is_control_free_and_bounded() {
        let reason = format!("secret\n{}", "x".repeat(PROBE_REASON_MAX_CHARS * 2));
        let bounded = bounded_probe_reason(reason);

        assert!(!bounded.chars().any(char::is_control));
        assert_eq!(bounded.chars().count(), PROBE_REASON_MAX_CHARS);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn runtime_probe_projects_agents_process_state_and_ports_from_one_source() {
        let source = FakeProbeSource::with_projection_fixture();

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![10])], &[(1, 7, 10)]);

        let pane_state = &snapshot.pane_process_states[&(1, 7)];
        assert!(pane_state.has_child_process);
        assert!(pane_state.remote_shell);
        assert_eq!(
            pane_state.ssh_command,
            Some(vec!["ssh".to_string(), "dev.ts".to_string()])
        );

        let pane_agent = &snapshot.pane_agents[&(1, 7)];
        assert!(pane_agent.running);
        assert_eq!(pane_agent.agent_name.as_deref(), Some("pi"));
        assert_eq!(pane_agent.session_id.as_deref(), Some("sess-1"));
        assert_eq!(snapshot.tab_agents[&1], *pane_agent);
        assert_eq!(snapshot.tab_ports[&1], vec![8080]);
        assert_eq!(snapshot.process_probe, ProbeState::Ok);
        assert_eq!(snapshot.ports_probe, ProbeState::Ok);
    }

    #[test]
    fn direct_ssh_probe_does_not_claim_remote_child_process_visibility() {
        let mut source = FakeProbeSource::default();
        source.comm.insert(50, "ssh".to_string());
        source
            .cmdline
            .insert(50, vec!["ssh".to_string(), "devbox".to_string()]);

        let snapshot = RuntimeProbeSnapshot::build(&source, 1_000, &[(1, vec![50])], &[(1, 7, 50)]);
        let pane_state = &snapshot.pane_process_states[&(1, 7)];

        assert!(pane_state.remote_shell);
        assert!(!pane_state.has_child_process);
    }

    #[test]
    fn test_probe_snapshot_equality_gates_ui_refresh() {
        let source = FakeProbeSource::with_projection_fixture();
        let tab_pids = vec![(1, vec![10])];
        let pane_pids = vec![(1, 7, 10)];

        // Same source + pids, different probe timestamp: renders identically, so
        // a clock-only delta must not force a sidebar refresh.
        let a = RuntimeProbeSnapshot::build(&source, 1_000, &tab_pids, &pane_pids);
        let b = RuntimeProbeSnapshot::build(&source, 5_000, &tab_pids, &pane_pids);
        assert!(a.renders_same_as(&b));

        // A listen table that maps the same socket inode to a different port
        // changes tab_ports, which is UI-relevant and must break render equality.
        let mut changed_source = FakeProbeSource::with_projection_fixture();
        changed_source.listen_table.insert(9001, 9090);
        let c = RuntimeProbeSnapshot::build(&changed_source, 1_000, &tab_pids, &pane_pids);
        assert!(!a.renders_same_as(&c));
    }

    #[test]
    fn test_probe_tick_coalesces_while_build_in_flight() {
        let f = ProbeInFlight::default();
        let g1 = f.try_begin();
        assert!(g1.is_some());
        // A tick that fires while the first build holds the guard is dropped.
        assert!(f.try_begin().is_none());
        drop(g1);
        // Once the in-flight build finishes, the next tick can begin again.
        assert!(f.try_begin().is_some());
    }
}
