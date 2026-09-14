//! Deterministic, headless workloads for release-profile performance checks.
//!
//! This module is available only with the `harness` feature. It deliberately
//! exercises shipped algorithms without constructing GTK widgets, so CI and
//! developers can compare the same fixtures on headless machines.

use std::cell::Cell;
use std::collections::HashMap;

use crate::agents::{AgentStatus, ListenTableProbe};
use crate::runtime_probe::{RuntimeProbeSnapshot, RuntimeProbeSource};

type TabPidList = Vec<(u32, Vec<i32>)>;
type PanePidList = Vec<(u32, u32, i32)>;

#[derive(Debug, Clone, Copy)]
pub struct WorkloadResult {
    pub checksum: u64,
    /// Total source reads across all iterations; divide by the iteration count
    /// to compare the number of reads performed by one scan.
    pub source_reads: usize,
}

#[derive(Default)]
struct SyntheticProbeSource {
    children: HashMap<i32, Vec<i32>>,
    comm: HashMap<i32, String>,
    cmdline: HashMap<i32, Vec<String>>,
    cwd: HashMap<i32, String>,
    socket_inodes: HashMap<i32, Vec<u64>>,
    listen_table: HashMap<u64, u16>,
    reads: Cell<usize>,
}

impl RuntimeProbeSource for SyntheticProbeSource {
    fn child_pids(&self, pid: i32) -> Vec<i32> {
        self.reads.set(self.reads.get() + 1);
        self.children.get(&pid).cloned().unwrap_or_default()
    }

    fn process_comm(&self, pid: i32) -> Option<String> {
        self.reads.set(self.reads.get() + 1);
        self.comm.get(&pid).cloned()
    }

    fn process_cmdline(&self, pid: i32) -> Option<Vec<String>> {
        self.reads.set(self.reads.get() + 1);
        self.cmdline.get(&pid).cloned()
    }

    fn process_cwd(&self, pid: i32) -> Option<String> {
        self.reads.set(self.reads.get() + 1);
        self.cwd.get(&pid).cloned()
    }

    fn socket_inodes(&self, pid: i32) -> Vec<u64> {
        self.reads.set(self.reads.get() + 1);
        self.socket_inodes.get(&pid).cloned().unwrap_or_default()
    }

    fn listen_table(&self) -> ListenTableProbe {
        self.reads.set(self.reads.get() + 1);
        ListenTableProbe::Complete(self.listen_table.clone())
    }
}

fn synthetic_probe_fixture() -> (SyntheticProbeSource, TabPidList, PanePidList) {
    const TABS: u32 = 8;
    const PANES_PER_TAB: u32 = 4;
    const PROCESSES_PER_PANE: i32 = 12;

    let mut source = SyntheticProbeSource::default();
    let mut tab_pids = Vec::with_capacity(TABS as usize);
    let mut pane_pids = Vec::with_capacity((TABS * PANES_PER_TAB) as usize);

    for tab_id in 1..=TABS {
        let mut roots = Vec::with_capacity(PANES_PER_TAB as usize);
        for pane_offset in 0..PANES_PER_TAB {
            let pane_id = pane_offset + 1;
            let root = (tab_id as i32 * 10_000) + (pane_id as i32 * 100);
            roots.push(root);
            pane_pids.push((tab_id, pane_id, root));
            source.cwd.insert(root, format!("/workspace/tab-{tab_id}"));

            for depth in 0..PROCESSES_PER_PANE {
                let pid = root + depth;
                if depth + 1 < PROCESSES_PER_PANE {
                    source.children.insert(pid, vec![pid + 1]);
                }
                let process = if depth == 1 {
                    if pane_id % 2 == 0 {
                        "codex"
                    } else {
                        "claude"
                    }
                } else {
                    "worker"
                };
                source.comm.insert(pid, process.to_string());
                source.cmdline.insert(
                    pid,
                    vec![
                        process.to_string(),
                        format!("--fixture={tab_id}-{pane_id}-{depth}"),
                    ],
                );
                let inode = (pid as u64) + 1_000_000;
                source.socket_inodes.insert(pid, vec![inode]);
                if depth == PROCESSES_PER_PANE - 1 {
                    source.listen_table.insert(inode, 3_000 + pane_id as u16);
                }
            }
        }
        tab_pids.push((tab_id, roots));
    }

    (source, tab_pids, pane_pids)
}

/// Exercise the periodic process/agent/port projection used by the GTK app.
pub fn runtime_probe(iterations: usize) -> WorkloadResult {
    let (source, tab_pids, pane_pids) = synthetic_probe_fixture();
    let mut checksum = 0_u64;
    for iteration in 0..iterations {
        let snapshot =
            RuntimeProbeSnapshot::build(&source, iteration as u64, &tab_pids, &pane_pids);
        checksum = checksum.wrapping_add(snapshot_checksum(&snapshot));
    }
    WorkloadResult {
        checksum,
        source_reads: source.reads.get(),
    }
}

fn snapshot_checksum(snapshot: &RuntimeProbeSnapshot) -> u64 {
    let mut checksum = SemanticChecksum::new();

    checksum.text("tab-pids");
    for (tab_id, pids) in &snapshot.tab_pids {
        checksum.u64(u64::from(*tab_id));
        checksum.i32_slice(pids);
    }

    checksum.text("pane-pids");
    for ((tab_id, pane_id), pid) in &snapshot.pane_pids {
        checksum.u64(u64::from(*tab_id));
        checksum.u64(u64::from(*pane_id));
        checksum.i32(*pid);
    }

    checksum.text("pane-agents");
    let mut pane_agents: Vec<_> = snapshot.pane_agents.iter().collect();
    pane_agents.sort_unstable_by_key(|(key, _)| **key);
    for ((tab_id, pane_id), status) in pane_agents {
        checksum.u64(u64::from(*tab_id));
        checksum.u64(u64::from(*pane_id));
        checksum.agent_status(status);
    }

    checksum.text("tab-agents");
    let mut tab_agents: Vec<_> = snapshot.tab_agents.iter().collect();
    tab_agents.sort_unstable_by_key(|(tab_id, _)| **tab_id);
    for (tab_id, status) in tab_agents {
        checksum.u64(u64::from(*tab_id));
        checksum.agent_status(status);
    }

    checksum.text("tab-ports");
    let mut tab_ports: Vec<_> = snapshot.tab_ports.iter().collect();
    tab_ports.sort_unstable_by_key(|(tab_id, _)| **tab_id);
    for (tab_id, ports) in tab_ports {
        checksum.u64(u64::from(*tab_id));
        checksum.u64(ports.len() as u64);
        for port in ports {
            checksum.u64(u64::from(*port));
        }
    }

    checksum.text("pane-process-states");
    let mut process_states: Vec<_> = snapshot.pane_process_states.iter().collect();
    process_states.sort_unstable_by_key(|(key, _)| **key);
    for ((tab_id, pane_id), state) in process_states {
        checksum.u64(u64::from(*tab_id));
        checksum.u64(u64::from(*pane_id));
        checksum.boolean(state.has_child_process);
        checksum.boolean(state.remote_shell);
        checksum.u64(state.ssh_command.as_ref().map_or(0, |args| args.len() + 1) as u64);
        if let Some(args) = &state.ssh_command {
            for arg in args {
                checksum.text(arg);
            }
        }
    }

    checksum.finish()
}

/// Stable FNV-1a checksum over the fixture's semantic projection.
///
/// `DefaultHasher` is intentionally avoided because Rust does not promise a
/// stable algorithm across toolchain versions. Length-prefixing strings and
/// slices keeps adjacent fields unambiguous.
struct SemanticChecksum(u64);

impl SemanticChecksum {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn i32(&mut self, value: i32) {
        self.u64(value as i64 as u64);
    }

    fn i32_slice(&mut self, values: &[i32]) {
        self.u64(values.len() as u64);
        for value in values {
            self.i32(*value);
        }
    }

    fn boolean(&mut self, value: bool) {
        self.u64(u64::from(value));
    }

    fn optional_text(&mut self, value: Option<&str>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.text(value);
        }
    }

    fn agent_status(&mut self, status: &AgentStatus) {
        self.optional_text(status.agent_name.as_deref());
        self.optional_text(status.session_id.as_deref());
        self.boolean(status.running);
    }

    fn finish(self) -> u64 {
        self.0
    }
}

fn session_json_fixture() -> String {
    let mut workspaces = Vec::new();
    for workspace in 0..8_u32 {
        let mut tabs = Vec::new();
        for tab in 0..16_u32 {
            let leaf = |pane: u32| {
                serde_json::json!({
                    "type": "leaf",
                    "work_origin": format!("pane-perf-{workspace}-{tab}-{pane}"),
                    "cwd": format!("/workspace/{workspace}/tab-{tab}/pane-{pane}")
                })
            };
            tabs.push(serde_json::json!({
                "name": format!("tab-{tab}"),
                "work_origin": format!("tab-perf-{workspace}-{tab}"),
                "cwd": format!("/workspace/{workspace}/tab-{tab}"),
                "panes": {
                    "type": "split",
                    "direction": "horizontal",
                    "ratio": 0.5,
                    "first": {
                        "type": "split",
                        "direction": "vertical",
                        "ratio": 0.5,
                        "first": leaf(0),
                        "second": leaf(1)
                    },
                    "second": {
                        "type": "split",
                        "direction": "vertical",
                        "ratio": 0.5,
                        "first": leaf(2),
                        "second": leaf(3)
                    }
                }
            }));
        }
        workspaces.push(serde_json::json!({
            "id": workspace + 1,
            "work_origin": format!("workspace-perf-{workspace}"),
            "name": format!("workspace-{workspace}"),
            "tabs": tabs,
            "active_tab_index": workspace as usize % 16
        }));
    }
    serde_json::json!({
        "version": 2,
        "workspaces": workspaces,
        "active_workspace_index": 3,
        "window_width": 1600,
        "window_height": 1000
    })
    .to_string()
}

/// Exercise session JSON parsing and origin normalization on a large restore.
pub fn session_restore(iterations: usize) -> WorkloadResult {
    let json = session_json_fixture();
    let mut checksum = 0_u64;
    for _ in 0..iterations {
        let mut state: crate::session::SessionStateV2 =
            serde_json::from_str(&json).expect("valid performance fixture");
        state.normalize_work_origins();
        checksum = checksum.wrapping_add(session_state_checksum(&state));
    }
    WorkloadResult {
        checksum,
        source_reads: 0,
    }
}

fn session_state_checksum(state: &crate::session::SessionStateV2) -> u64 {
    let encoded = serde_json::to_vec(state).expect("serialize normalized performance fixture");
    let mut checksum = SemanticChecksum::new();
    checksum.text("session-state");
    checksum.bytes(&encoded);
    checksum.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_snapshot() -> RuntimeProbeSnapshot {
        let (source, tab_pids, pane_pids) = synthetic_probe_fixture();
        RuntimeProbeSnapshot::build(&source, 0, &tab_pids, &pane_pids)
    }

    #[test]
    fn snapshot_checksum_covers_agent_identity_session_ports_and_pane_mapping() {
        let snapshot = fixture_snapshot();
        let expected = snapshot_checksum(&snapshot);

        let mut changed = snapshot.clone();
        changed
            .pane_agents
            .get_mut(&(1, 1))
            .expect("fixture pane agent")
            .agent_name = Some("different-agent".to_string());
        assert_ne!(snapshot_checksum(&changed), expected);

        let mut changed = snapshot.clone();
        changed
            .pane_agents
            .get_mut(&(1, 1))
            .expect("fixture pane agent")
            .session_id = Some("different-session".to_string());
        assert_ne!(snapshot_checksum(&changed), expected);

        let mut changed = snapshot.clone();
        changed.tab_ports.get_mut(&1).expect("fixture tab ports")[0] += 1;
        assert_ne!(snapshot_checksum(&changed), expected);

        let mut changed = snapshot.clone();
        let status = changed
            .pane_agents
            .remove(&(1, 1))
            .expect("fixture pane agent");
        changed.pane_agents.insert((1, 99), status);
        assert_ne!(snapshot_checksum(&changed), expected);
    }

    #[test]
    fn session_checksum_covers_normalized_content_not_only_topology() {
        let json = session_json_fixture();
        let mut state: crate::session::SessionStateV2 =
            serde_json::from_str(&json).expect("valid performance fixture");
        state.normalize_work_origins();
        let expected = session_state_checksum(&state);

        state.workspaces[0].work_origin = Some("workspace-different".to_string());
        assert_ne!(session_state_checksum(&state), expected);
    }
}
