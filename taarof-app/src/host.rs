/// Per-host configuration for tmux session management.
#[derive(Clone, Debug)]
pub struct HostConfig {
    pub name: String,
    pub ssh_target: Option<String>,
    pub max_sessions: u32,
    pub warn_cpu_percent: u32,
    pub warn_memory_percent: u32,
    pub idle_detach_minutes: u32,
    pub tmux_backed: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            ssh_target: None,
            max_sessions: 8,
            warn_cpu_percent: 80,
            warn_memory_percent: 80,
            idle_detach_minutes: 120,
            tmux_backed: true,
        }
    }
}

/// Snapshot of a host's resource usage.
#[derive(Clone, Debug, PartialEq)]
pub struct HostStatus {
    pub session_count: u32,
    pub cpu_load_percent: f64,
    pub memory_used_percent: f64,
}

/// Result of checking a host's session budget.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub enum BudgetResult {
    Ok,
    AtLimit { current: u32, max: u32 },
    ResourceWarning { cpu: bool, memory: bool },
}

/// Check whether a host is within its configured resource budget.
///
/// Priority: session limit is checked first; resource warnings are only
/// returned when the session count is still under the limit.
#[cfg(test)]
pub fn check_budget(config: &HostConfig, status: &HostStatus) -> BudgetResult {
    if status.session_count >= config.max_sessions {
        return BudgetResult::AtLimit {
            current: status.session_count,
            max: config.max_sessions,
        };
    }

    let cpu_warn = status.cpu_load_percent > f64::from(config.warn_cpu_percent);
    let mem_warn = status.memory_used_percent > f64::from(config.warn_memory_percent);

    if cpu_warn || mem_warn {
        return BudgetResult::ResourceWarning {
            cpu: cpu_warn,
            memory: mem_warn,
        };
    }

    BudgetResult::Ok
}

/// Build the argv for a remote probe command over SSH.
///
/// Uses BatchMode and ConnectTimeout to avoid interactive prompts and hangs.
/// The returned vector contains:
/// `["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", ssh_target, "..."]`
pub fn probe_status_command(ssh_target: &str) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=5".to_string(),
        ssh_target.to_string(),
        "cat /proc/loadavg; free -m | grep Mem; tmux list-sessions 2>/dev/null | wc -l; nproc"
            .to_string(),
    ]
}

/// Parse the stdout produced by the probe command into a `HostStatus`.
///
/// Expected line layout:
/// - Line 0: `/proc/loadavg` — e.g. `1.23 0.98 0.76 2/345 12345`
/// - Line 1: `free -m` Mem line — e.g. `Mem:  16000  10400   3200 …`
/// - Line 2 (optional): tmux session count (integer)
/// - Line 3 (optional): nproc (defaults to 4 if missing)
///
/// Returns `None` if the required lines cannot be parsed.
pub fn parse_probe_output(output: &str) -> Option<HostStatus> {
    let mut lines = output.lines();

    // Line 0: load average
    let load_line = lines.next()?;
    let load_1m: f64 = load_line.split_whitespace().next()?.parse().ok()?;

    // Line 1: memory (free -m Mem: line)
    let mem_line = lines.next()?;
    let mut mem_fields = mem_line.split_whitespace();
    // field 0 is "Mem:", field 1 is total, field 2 is used
    mem_fields.next()?; // skip "Mem:"
    let mem_total: f64 = mem_fields.next()?.parse().ok()?;
    let mem_used: f64 = mem_fields.next()?.parse().ok()?;

    if mem_total == 0.0 {
        return None;
    }

    let session_count_line = lines.next().map(str::trim);
    let nproc_line = lines.next().map(str::trim);

    // Line 2 (optional): session count. This line is advisory; degrade to zero
    // on malformed output so CPU and memory data still survive noisy shells.
    let session_count: u32 = session_count_line
        .and_then(|line| line.parse().ok())
        .unwrap_or(0);

    // Line 3 (optional): nproc
    let nproc: f64 = match nproc_line {
        Some(line) => {
            let nproc: f64 = line.trim().parse().ok()?;
            if !nproc.is_finite() || nproc <= 0.0 {
                return None;
            }
            nproc
        }
        None => 4.0,
    };

    let cpu_load_percent = (load_1m / nproc) * 100.0;
    let memory_used_percent = (mem_used / mem_total) * 100.0;

    Some(HostStatus {
        session_count,
        cpu_load_percent,
        memory_used_percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_host_config() {
        let cfg = HostConfig::default();
        assert_eq!(cfg.max_sessions, 8);
        assert_eq!(cfg.warn_cpu_percent, 80);
        assert_eq!(cfg.warn_memory_percent, 80);
        assert_eq!(cfg.idle_detach_minutes, 120);
        assert!(cfg.tmux_backed);
        assert!(cfg.ssh_target.is_none());
        assert!(cfg.name.is_empty());
    }

    #[test]
    fn test_budget_ok_when_under_limit() {
        let cfg = HostConfig {
            max_sessions: 6,
            ..Default::default()
        };
        let status = HostStatus {
            session_count: 3,
            cpu_load_percent: 10.0,
            memory_used_percent: 20.0,
        };
        assert_eq!(check_budget(&cfg, &status), BudgetResult::Ok);
    }

    #[test]
    fn test_budget_warn_when_at_limit() {
        let cfg = HostConfig {
            max_sessions: 6,
            ..Default::default()
        };
        let status = HostStatus {
            session_count: 6,
            cpu_load_percent: 10.0,
            memory_used_percent: 20.0,
        };
        assert_eq!(
            check_budget(&cfg, &status),
            BudgetResult::AtLimit { current: 6, max: 6 }
        );
    }

    #[test]
    fn test_budget_warn_high_cpu() {
        let cfg = HostConfig {
            warn_cpu_percent: 70,
            ..Default::default()
        };
        let status = HostStatus {
            session_count: 2,
            cpu_load_percent: 85.0,
            memory_used_percent: 30.0,
        };
        assert_eq!(
            check_budget(&cfg, &status),
            BudgetResult::ResourceWarning {
                cpu: true,
                memory: false
            }
        );
    }

    #[test]
    fn test_budget_warn_high_memory() {
        let cfg = HostConfig {
            warn_memory_percent: 80,
            ..Default::default()
        };
        let status = HostStatus {
            session_count: 2,
            cpu_load_percent: 10.0,
            memory_used_percent: 92.0,
        };
        assert_eq!(
            check_budget(&cfg, &status),
            BudgetResult::ResourceWarning {
                cpu: false,
                memory: true
            }
        );
    }

    #[test]
    fn test_parse_probe_output() {
        // 4 nproc, load 1.23 → cpu = (1.23 / 4) * 100 = 30.75
        // mem total 16000, used 10400 → memory = (10400 / 16000) * 100 = 65.0
        let output = "1.23 0.98 0.76 2/345 12345\nMem:  16000  10400   3200    200   2400  5400\n";
        let status = parse_probe_output(output).expect("should parse");
        assert!((status.cpu_load_percent - 30.75).abs() < 0.01);
        assert!((status.memory_used_percent - 65.0).abs() < 0.01);
        assert_eq!(status.session_count, 0); // no line 2
    }

    #[test]
    fn test_parse_probe_output_malformed() {
        assert!(parse_probe_output("garbage").is_none());
        assert!(parse_probe_output("").is_none());
    }

    #[test]
    fn test_remote_host_snapshot_smoke_parses_remote_probe_output() {
        let output = "\
2.40 1.80 1.20 3/900 12345
Mem:  32000  16000   4000    200   12000  14000
5
8
";
        let status = parse_probe_output(output).expect("valid remote probe output should parse");

        assert_eq!(status.session_count, 5);
        assert!((status.cpu_load_percent - 30.0).abs() < 0.01);
        assert!((status.memory_used_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_remote_host_snapshot_smoke_rejects_malformed_probe_output() {
        assert!(parse_probe_output("1.00 0.50 0.25 1/1 1\nMem: nope\n2\n4\n").is_none());
        assert!(parse_probe_output("1.00 0.50 0.25 1/1 1\nMem: 0 0 0\n2\n4\n").is_none());
    }

    #[test]
    fn test_remote_host_snapshot_smoke_tolerates_invalid_session_count_line() {
        let output = "\
1.00 0.50 0.25 1/1 1
Mem:  16000  8000   4000    200   2400  5400
not-a-count
4
";

        let status = parse_probe_output(output).expect("invalid session count should degrade");
        assert_eq!(status.session_count, 0);
        assert!((status.cpu_load_percent - 25.0).abs() < 0.01);
        assert!((status.memory_used_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_remote_host_snapshot_smoke_rejects_invalid_nproc_line() {
        let output = "\
1.00 0.50 0.25 1/1 1
Mem:  16000  8000   4000    200   2400  5400
2
not-a-number
";

        assert!(parse_probe_output(output).is_none());
    }

    #[test]
    fn test_remote_host_snapshot_smoke_rejects_non_positive_nproc() {
        let output = "\
1.00 0.50 0.25 1/1 1
Mem:  16000  8000   4000    200   2400  5400
2
0
";

        assert!(parse_probe_output(output).is_none());
    }

    #[test]
    fn test_probe_command() {
        let argv = probe_status_command("user@devbox");
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-o");
        assert_eq!(argv[2], "BatchMode=yes");
        assert_eq!(argv[3], "-o");
        assert_eq!(argv[4], "ConnectTimeout=5");
        assert_eq!(argv[5], "user@devbox");
        assert_eq!(
            argv[6],
            "cat /proc/loadavg; free -m | grep Mem; tmux list-sessions 2>/dev/null | wc -l; nproc"
        );
        assert_eq!(argv.len(), 7);
    }
}
