//! Process-tree scanning for the agent sidecar.
//!
//! This module owns walking `/proc` to find agent processes, matching
//! command lines against the loaded signature catalogue, and building
//! the listening-ports table for the sidebar.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use super::signatures::{merged_agent_signatures_for_cwd, AgentSignature};
use super::AgentStatus;

pub(crate) type ProcessFact = (i32, Option<String>, Option<Vec<String>>);

fn build_agent_status(agent_name: String, _pid: i32, cmdline: Option<&[String]>) -> AgentStatus {
    let session_id = cmdline.and_then(|cmdline| extract_agent_session_id(&agent_name, cmdline));
    AgentStatus {
        agent_name: Some(agent_name),
        session_id,
        running: true,
    }
}

/// Get direct child PIDs of a process via `/proc`.
///
/// Linux attributes a child to the specific thread that spawned it. Reading
/// only `/proc/<pid>/task/<pid>/children` therefore misses children launched
/// by worker threads (notably agents started by `infisical`). Collect every
/// task's children so the pane process tree retains all agent instances.
pub(crate) fn get_child_pids(pid: i32) -> Vec<i32> {
    get_child_pids_from(Path::new("/proc"), pid)
}

/// Read the root facts required to trust a runtime process projection.
///
/// Unlike the best-effort descendant walk, a missing or unreadable pane root
/// means the process source did not produce healthy empty truth. Reasons are
/// deliberately stable and value-free so they are safe to expose through the
/// diagnostics and query APIs.
pub(crate) fn try_get_root_process_facts(pid: i32) -> Result<(Vec<i32>, Option<String>), String> {
    get_root_process_facts_from(Path::new("/proc"), pid)
}

fn get_root_process_facts_from(
    proc_root: &Path,
    pid: i32,
) -> Result<(Vec<i32>, Option<String>), String> {
    let task_dir = proc_root.join(pid.to_string()).join("task");
    let tasks = fs::read_dir(task_dir).map_err(process_root_read_reason)?;
    let mut children = BTreeSet::new();
    for task in tasks {
        let task = task.map_err(process_root_read_reason)?;
        match fs::read_to_string(task.path().join("children")) {
            Ok(content) => children.extend(
                content
                    .split_whitespace()
                    .filter_map(|value| value.parse::<i32>().ok()),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Threads can exit between read_dir and reading their children.
            }
            Err(error) => return Err(process_root_read_reason(error)),
        }
    }

    let comm = fs::read_to_string(proc_root.join(pid.to_string()).join("comm"))
        .map_err(process_root_read_reason)?
        .trim()
        .to_string();
    Ok((children.into_iter().collect(), Some(comm)))
}

fn process_root_read_reason(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "process root unavailable",
        std::io::ErrorKind::PermissionDenied => "process source unreadable",
        _ => "process source unavailable",
    }
    .to_string()
}

fn get_child_pids_from(proc_root: &Path, pid: i32) -> Vec<i32> {
    let task_dir = proc_root.join(pid.to_string()).join("task");
    let Ok(tasks) = fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut children = BTreeSet::new();
    for task in tasks.flatten() {
        let Ok(content) = fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        children.extend(
            content
                .split_whitespace()
                .filter_map(|value| value.parse::<i32>().ok()),
        );
    }
    children.into_iter().collect()
}

/// Read the process name from /proc/<pid>/comm.
pub(crate) fn get_process_comm(pid: i32) -> Option<String> {
    let path = format!("/proc/{pid}/comm");
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Read the full command line from /proc/<pid>/cmdline (NUL-separated).
pub(crate) fn get_process_cmdline(pid: i32) -> Option<Vec<String>> {
    let path = format!("/proc/{pid}/cmdline");
    let content = fs::read(path).ok()?;
    let args: Vec<String> = content
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

pub(crate) fn get_process_cwd(pid: i32) -> Option<String> {
    let path = format!("/proc/{pid}/cwd");
    std::fs::read_link(path)
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned())
}

const SSH_NAMES: &[&str] = &["ssh", "autossh", "mosh", "mosh-client"];

pub(crate) fn is_ssh_process(name: &str) -> bool {
    SSH_NAMES.contains(&name)
}

/// Classify a single process, applying the same precedence the tree scan uses:
/// identity-bearing matches first, then a broad substring match.
///
/// Production code always has a whole process tree in hand and goes through
/// [`detect_agent_in_process_facts`], which must run each pass across the full
/// tree; this single-process form exists for tests of the matching rules.
#[cfg(test)]
pub(super) fn match_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
    signatures: &[AgentSignature],
) -> Option<String> {
    let signatures = PreparedAgentSignatures::new(signatures);
    match_strong_agent_signature(process_name, cmdline, &signatures)
        .or_else(|| match_broad_agent_signature(process_name, cmdline, &signatures))
}

/// Substring match of signature patterns against the process name and the
/// command-shaped part of its argv. Deliberately loose — it is the last resort
/// after exact identity has failed across the whole tree.
fn match_broad_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
    signatures: &PreparedAgentSignatures<'_>,
) -> Option<String> {
    let process_name = process_name.unwrap_or("").to_lowercase();
    let cmdline = command_shaped_text(cmdline);

    for signature in &signatures.signatures {
        if signature.patterns.iter().any(|pattern| {
            process_name.contains(&pattern.lowercase) || cmdline.contains(&pattern.lowercase)
        }) {
            return Some(signature.name.to_string());
        }
    }
    None
}

#[derive(Debug)]
struct PreparedPattern {
    lowercase: String,
    basename: String,
}

#[derive(Debug)]
struct PreparedAgentSignature<'a> {
    name: &'a str,
    patterns: Vec<PreparedPattern>,
}

#[derive(Debug)]
struct PreparedAgentSignatures<'a> {
    signatures: Vec<PreparedAgentSignature<'a>>,
}

impl<'a> PreparedAgentSignatures<'a> {
    fn new(signatures: &'a [AgentSignature]) -> Self {
        Self {
            signatures: signatures
                .iter()
                .map(|signature| PreparedAgentSignature {
                    name: &signature.name,
                    patterns: signature
                        .patterns
                        .iter()
                        .map(|pattern| PreparedPattern {
                            lowercase: pattern.to_lowercase(),
                            basename: command_basename(pattern),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Join the argv entries that can plausibly *name* a command, lowercased.
///
/// Launchers routinely carry an entire shell program as one argv element
/// (`infisical run ... -- bash -lc '<script>'`). That text is data, not
/// identity: the Pi launcher's script mentions Codex in a comment and exports
/// `CODEX_GITHUB_PERSONAL_ACCESS_TOKEN`, which a naive substring match reads as
/// "this pane is Codex". Embedded script bodies contain whitespace, while
/// `env`-style assignments contain a valid variable name followed by `=`;
/// neither is executable identity.
fn command_shaped_text(cmdline: Option<&[String]>) -> String {
    cmdline
        .map(|args| {
            args.iter()
                .filter(|arg| is_command_shaped(arg))
                .map(|arg| arg.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn is_command_shaped(arg: &str) -> bool {
    !arg.is_empty() && !arg.chars().any(char::is_whitespace) && !is_environment_assignment(arg)
}

fn is_environment_assignment(arg: &str) -> bool {
    let Some((name, _)) = arg.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn detect_builtin_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
) -> Option<String> {
    for candidate in command_identity_candidates(process_name, cmdline) {
        match candidate.as_str() {
            "pi" | "pii" => return Some("pi".to_string()),
            _ => {}
        }
    }
    None
}

fn command_identity_candidates(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
) -> Vec<String> {
    process_name
        .into_iter()
        .chain(cmdline.into_iter().flatten().take(2).map(String::as_str))
        .map(command_basename)
        .collect()
}

fn command_basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase()
}

/// Match a process against signatures by *executable identity*: the process
/// name, the first two argv entries, and whatever a wrapper hands over after a
/// bare `--` separator, each reduced to a basename and compared for equality.
///
/// Only a signature's explicit patterns are treated as executable names; its
/// display name is not, so `{ name = "helper", patterns = ["my-agent"] }` never
/// claims a process that merely happens to be called `helper`.
///
/// Configured signatures are consulted before the built-in alias table so a
/// user catalogue can reclassify a process the defaults already know about.
#[cfg(test)]
fn match_exact_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
    signatures: &[AgentSignature],
) -> Option<String> {
    let signatures = PreparedAgentSignatures::new(signatures);
    match_exact_prepared_agent_signature(process_name, cmdline, &signatures)
}

fn match_exact_prepared_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
    signatures: &PreparedAgentSignatures<'_>,
) -> Option<String> {
    let mut candidates = command_identity_candidates(process_name, cmdline);
    if let Some(cmdline) = cmdline {
        candidates.extend(
            cmdline
                .windows(2)
                .filter(|args| args[0] == "--")
                .map(|args| command_basename(&args[1])),
        );
    }
    signatures
        .signatures
        .iter()
        .find_map(|signature| {
            signature
                .patterns
                .iter()
                .any(|pattern| candidates.contains(&pattern.basename))
                .then(|| signature.name.to_string())
        })
        .or_else(|| detect_builtin_agent_signature(process_name, cmdline))
}

/// Exact identity, or the next best thing: a signature pattern embedded in the
/// *process name* on token boundaries (`my-agent-wrapper` is `my-agent`). The
/// longest matching pattern wins, so a `codex-review` signature is not stolen by
/// a shorter `codex` one.
///
/// This stays identity-bearing because it only reads the executable's own name,
/// never argv, so a wrapper's script text cannot reach it.
fn match_strong_agent_signature(
    process_name: Option<&str>,
    cmdline: Option<&[String]>,
    signatures: &PreparedAgentSignatures<'_>,
) -> Option<String> {
    if let Some(agent_name) =
        match_exact_prepared_agent_signature(process_name, cmdline, signatures)
    {
        return Some(agent_name);
    }

    // `/proc/<pid>/comm` can disappear for a short-lived process while its
    // argv is still readable. In that case argv[0] remains executable
    // identity; later argv entries do not, because they may be script bodies
    // or file operands.
    let executable =
        process_name.or_else(|| cmdline.and_then(|args| args.first()).map(String::as_str))?;
    let process_name = command_basename(executable);
    signatures
        .signatures
        .iter()
        .flat_map(|signature| {
            signature.patterns.iter().filter_map(|pattern| {
                contains_identity_pattern(&process_name, &pattern.basename)
                    .then_some((pattern.basename.len(), signature.name))
            })
        })
        .max_by_key(|(pattern_len, _)| *pattern_len)
        .map(|(_, agent_name)| agent_name.to_string())
}

fn contains_identity_pattern(candidate: &str, pattern: &str) -> bool {
    candidate.match_indices(pattern).any(|(start, _)| {
        let end = start + pattern.len();
        let is_boundary = |byte: u8| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'.');
        (start == 0 || is_boundary(candidate.as_bytes()[start - 1]))
            && (end == candidate.len() || is_boundary(candidate.as_bytes()[end]))
    })
}

pub(super) fn extract_agent_session_id(agent_name: &str, cmdline: &[String]) -> Option<String> {
    let supports_kimi_short_flag = agent_name.eq_ignore_ascii_case("kimi");
    let mut iter = cmdline.iter().peekable();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--resume=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        if let Some(value) = arg.strip_prefix("--session=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        if supports_kimi_short_flag {
            if let Some(value) = arg.strip_prefix("-S=") {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        if matches!(arg.as_str(), "--resume" | "--session" | "resume")
            || (supports_kimi_short_flag && arg == "-S")
        {
            if let Some(next) = iter.peek() {
                if !next.is_empty() && !next.starts_with('-') {
                    return Some((**next).clone());
                }
            }
        }
    }
    None
}

pub(crate) fn detect_agent_in_process_facts(
    cwd: Option<&str>,
    processes: &[ProcessFact],
) -> AgentStatus {
    let signatures = merged_agent_signatures_for_cwd(cwd);
    detect_agent_in_process_facts_with_signatures(processes, &signatures)
}

pub(super) fn detect_agent_in_process_facts_with_signatures(
    processes: &[ProcessFact],
    signatures: &[AgentSignature],
) -> AgentStatus {
    // Signature patterns come from static/global and project configuration, but
    // this scan can inspect hundreds of processes. Normalize each pattern once
    // per scan instead of allocating lowercase basenames inside every
    // process-by-pattern comparison.
    let signatures = PreparedAgentSignatures::new(signatures);

    // Identity precedence, evaluated one whole pass at a time so a weaker match
    // on *any* process can never outrank a stronger match on *another* one:
    //
    //   1. identity-bearing matches anywhere in the tree (breadth-first, so the
    //      owning process wins over its tool subprocesses): exact executable
    //      identity, then a signature pattern embedded in the process name,
    //   2. broad substring match over command-shaped text anywhere in the tree.
    //
    // Pass 1 keeps shallow, parent-first ownership for identity-bearing matches,
    // so a `my-agent-wrapper` process still owns the pane over the `codex` tool
    // subprocess it spawned. Broad argv substrings stay in pass 2 because launch
    // wrappers mention provider credentials before the real agent appears: a
    // launcher such as `pi-run` -> `infisical run -- bash -lc <script>` -> `pi`
    // only resolves correctly if pass 1 covers the entire tree before pass 2
    // gets to read the wrapper's script text.
    for (pid, process_name, cmdline) in processes {
        if let Some(agent_name) =
            match_strong_agent_signature(process_name.as_deref(), cmdline.as_deref(), &signatures)
        {
            return build_agent_status(agent_name, *pid, cmdline.as_deref());
        }
    }

    for (pid, process_name, cmdline) in processes {
        if let Some(agent_name) =
            match_broad_agent_signature(process_name.as_deref(), cmdline.as_deref(), &signatures)
        {
            return build_agent_status(agent_name, *pid, cmdline.as_deref());
        }
    }

    AgentStatus {
        agent_name: None,
        session_id: None,
        running: false,
    }
}

/// Parse a single /proc/net/tcp line. Returns (inode, port) if the line is a LISTEN socket.
pub(super) fn parse_tcp_listen_line(line: &str) -> Option<(u64, u16)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 {
        return None;
    }
    // fields[3] is connection state — "0A" means LISTEN
    if fields[3] != "0A" {
        return None;
    }
    // fields[1] is "ADDR:PORT" in hex
    let port_hex = fields[1].split(':').nth(1)?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    // fields[9] is the socket inode
    let inode = fields[9].parse::<u64>().ok()?;
    Some((inode, port))
}

/// Parse a single /proc/net/udp or /proc/net/udp6 line.
/// Returns (inode, port) if the socket is a bound/unconnected UDP listener.
///
/// In the kernel's UDP table the state column (fields[3]) is `07` (TCP_CLOSE,
/// repurposed) for unconnected sockets, which are the UDP equivalent of a
/// listening port. Connected UDP sockets show a non-zero remote address and
/// state `01`. We identify listeners by requiring state `07`.
pub(super) fn parse_udp_listen_line(line: &str) -> Option<(u64, u16)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 {
        return None;
    }
    // State "07" = unconnected (UDP listener equivalent)
    if fields[3] != "07" {
        return None;
    }
    // fields[1] is "ADDR:PORT" in hex; skip if port is zero
    let port_hex = fields[1].split(':').nth(1)?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    if port == 0 {
        return None;
    }
    // fields[9] is the socket inode
    let inode = fields[9].parse::<u64>().ok()?;
    Some((inode, port))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ListenTableProbe {
    Complete(HashMap<u64, u16>),
    Partial {
        table: HashMap<u64, u16>,
        error: String,
    },
    Failed {
        error: String,
    },
}

type ListenLineParser = fn(&str) -> Option<(u64, u16)>;

/// Read /proc/net/tcp{,6} and /proc/net/udp{,6}, preserving both the data and
/// failure truth when only a subset of the four kernel tables is readable.
pub(crate) fn try_build_listen_table() -> ListenTableProbe {
    build_listen_table_with(|path| fs::read_to_string(path))
}

fn build_listen_table_with(
    mut read_table: impl FnMut(&str) -> std::io::Result<String>,
) -> ListenTableProbe {
    let mut table = HashMap::new();
    let mut tables_read = 0usize;
    let mut errors = Vec::new();

    for (path, parse_line) in [
        ("/proc/net/tcp", parse_tcp_listen_line as ListenLineParser),
        ("/proc/net/tcp6", parse_tcp_listen_line as ListenLineParser),
        ("/proc/net/udp", parse_udp_listen_line as ListenLineParser),
        ("/proc/net/udp6", parse_udp_listen_line as ListenLineParser),
    ] {
        let content = match read_table(path) {
            Ok(content) => {
                tables_read += 1;
                content
            }
            Err(error) => {
                errors.push(format!("{path}: {error}"));
                continue;
            }
        };
        for line in content.lines().skip(1) {
            if let Some((inode, port)) = parse_line(line) {
                table.entry(inode).or_insert(port);
            }
        }
    }

    if errors.is_empty() {
        ListenTableProbe::Complete(table)
    } else {
        let error = format!(
            "{} of 4 /proc/net tables unreadable: {}",
            errors.len(),
            errors.join("; ")
        );
        if tables_read == 0 {
            ListenTableProbe::Failed { error }
        } else {
            ListenTableProbe::Partial { table, error }
        }
    }
}

#[cfg(test)]
mod listen_table_tests {
    use super::{build_listen_table_with, ListenTableProbe};
    use std::io;

    const TCP_LISTENER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n";

    #[test]
    fn listen_table_partial_read_preserves_data_and_reports_every_failed_table() {
        let probe = build_listen_table_with(|path| match path {
            "/proc/net/tcp" => Ok(TCP_LISTENER.to_string()),
            "/proc/net/tcp6" => Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            "/proc/net/udp" => Err(io::Error::other("read failed")),
            "/proc/net/udp6" => Err(io::Error::new(io::ErrorKind::PermissionDenied, "blocked")),
            unexpected => panic!("unexpected table {unexpected}"),
        });

        let ListenTableProbe::Partial { table, error } = probe else {
            panic!("one readable table must be represented as partial truth");
        };
        assert_eq!(table.get(&12345), Some(&3000));
        assert!(error.contains("3 of 4 /proc/net tables unreadable"));
        assert!(error.contains("/proc/net/tcp6"));
        assert!(error.contains("/proc/net/udp"));
        assert!(error.contains("/proc/net/udp6"));
    }

    #[test]
    fn listen_table_all_reads_failed_is_not_a_healthy_empty_table() {
        let probe = build_listen_table_with(|path| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{path} denied"),
            ))
        });

        let ListenTableProbe::Failed { error } = probe else {
            panic!("zero readable tables must be a failed probe");
        };
        assert!(error.contains("4 of 4 /proc/net tables unreadable"));
    }
}

/// Collect all PIDs in a process tree (including the root PID itself).
#[cfg(test)]
pub(super) fn collect_process_tree(pid: i32) -> Vec<i32> {
    let mut tree = vec![pid];
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        for child in get_child_pids(p) {
            tree.push(child);
            stack.push(child);
        }
    }
    tree
}

/// Read all socket inodes from /proc/{pid}/fd/ symlinks.
pub(crate) fn get_socket_inodes(pid: i32) -> Vec<u64> {
    let fd_path = format!("/proc/{pid}/fd");
    let Ok(entries) = fs::read_dir(&fd_path) else {
        return Vec::new();
    };
    let mut inodes = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let target_str = target.to_string_lossy();
        // Format: "socket:[12345]"
        if let Some(rest) = target_str.strip_prefix("socket:[") {
            if let Some(inode_str) = rest.strip_suffix(']') {
                if let Ok(inode) = inode_str.parse::<u64>() {
                    inodes.push(inode);
                }
            }
        }
    }
    inodes
}

/// Format a list of ports for display in the sidebar.
/// Shows up to 5 ports as `:3000 :8080`, with overflow as `+N`.
pub fn format_ports_label(ports: &[u16]) -> String {
    const MAX_DISPLAY: usize = 5;
    if ports.is_empty() {
        return String::new();
    }
    let display_count = ports.len().min(MAX_DISPLAY);
    let mut label: String = ports[..display_count]
        .iter()
        .map(|p| format!(":{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    let overflow = ports.len().saturating_sub(MAX_DISPLAY);
    if overflow > 0 {
        label.push_str(&format!(" +{overflow}"));
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::signatures::default_agent_signatures;

    /// The `bash -lc` program the real `pi-run` launcher hands to
    /// `infisical run`, reproduced (trimmed) from a live process tree.
    ///
    /// It names Codex in a comment and in an exported variable, and Kimi in a
    /// comment, while the process that actually owns the pane is Pi.
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

    fn args(values: &[&str]) -> Option<Vec<String>> {
        Some(values.iter().map(|value| (*value).to_string()).collect())
    }

    #[test]
    fn child_discovery_reads_children_spawned_by_non_leader_threads() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let proc_root = std::env::temp_dir().join(format!(
            "taarof-proc-children-{}-{unique}",
            std::process::id()
        ));
        let task_root = proc_root.join("100/task");
        for task_id in [100, 101, 102] {
            fs::create_dir_all(task_root.join(task_id.to_string())).expect("task fixture");
        }
        fs::write(task_root.join("100/children"), "200 201\n").expect("leader children");
        fs::write(task_root.join("101/children"), "202\n").expect("worker children");
        fs::write(task_root.join("102/children"), "201 203\n").expect("deduplicated children");

        assert_eq!(
            get_child_pids_from(&proc_root, 100),
            vec![200, 201, 202, 203]
        );

        fs::remove_dir_all(proc_root).expect("remove proc fixture");
    }

    #[test]
    fn runtime_probe_failure_unreadable_process_root_is_bounded_truth() {
        let missing_root = Path::new("/definitely-not-a-proc-fixture");

        assert_eq!(
            get_root_process_facts_from(missing_root, 100),
            Err("process root unavailable".to_string())
        );
    }

    fn custom_signatures() -> Vec<AgentSignature> {
        vec![
            AgentSignature {
                name: "helper".to_string(),
                patterns: vec!["my-custom-agent".to_string()],
            },
            AgentSignature {
                name: "codex".to_string(),
                patterns: vec!["codex".to_string()],
            },
            AgentSignature {
                name: "claude".to_string(),
                patterns: vec!["claude".to_string()],
            },
        ]
    }

    fn infisical_wrapper() -> ProcessFact {
        (
            102,
            Some("infisical".to_string()),
            args(&[
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
        )
    }

    /// pane shell -> `bash pi-run` -> `infisical run -- bash -lc <script>` -> `pi`,
    /// matching the process hierarchy observed on a live host.
    fn pi_run_process_tree(pi_started: bool) -> Vec<ProcessFact> {
        let mut tree = vec![
            (100, Some("zsh".to_string()), args(&["-zsh"])),
            (
                101,
                Some("bash".to_string()),
                args(&["bash", "/tmp/user/.local/bin/pi-run"]),
            ),
            infisical_wrapper(),
        ];
        if pi_started {
            tree.push((
                103,
                Some("pi".to_string()),
                args(&["/tmp/user/.local/share/mise/installs/pi/latest/pi/pi"]),
            ));
        }
        tree
    }

    #[test]
    fn pi_run_infisical_tree_resolves_to_pi_not_codex() {
        let status = detect_agent_in_process_facts_with_signatures(
            &pi_run_process_tree(true),
            &default_agent_signatures(),
        );

        assert_eq!(status.agent_name.as_deref(), Some("pi"));
        assert!(status.running);
    }

    #[test]
    fn pi_run_wrapper_without_pi_yet_reports_no_agent() {
        let status = detect_agent_in_process_facts_with_signatures(
            &pi_run_process_tree(false),
            &default_agent_signatures(),
        );

        assert_eq!(
            status.agent_name, None,
            "an unresolved launcher must stay unlabelled rather than claim the \
             agent its wrapper script happens to mention"
        );
        assert!(!status.running);
    }

    #[test]
    fn wrapper_script_body_never_drives_broad_matching() {
        assert_eq!(
            match_agent_signature(
                infisical_wrapper().1.as_deref(),
                infisical_wrapper().2.as_deref(),
                &default_agent_signatures(),
            ),
            None
        );
    }

    #[test]
    fn explicit_patterns_do_not_treat_signature_name_as_an_executable() {
        assert_eq!(
            match_exact_agent_signature(Some("helper"), None, &custom_signatures()),
            None
        );
    }

    #[test]
    fn custom_wrapper_identity_outranks_codex_tool_subprocess() {
        let processes = vec![
            (
                10,
                Some("my-custom-agent-wrapper".to_string()),
                args(&["my-custom-agent-wrapper"]),
            ),
            (
                11,
                Some("codex".to_string()),
                args(&["codex", "app-server"]),
            ),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &custom_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("helper"));
    }

    #[test]
    fn argv_zero_wrapper_identity_is_strong_when_process_name_is_missing() {
        let processes = vec![
            (
                10,
                None,
                Some(vec!["/usr/bin/my-custom-agent-wrapper".to_string()]),
            ),
            (
                11,
                Some("codex".to_string()),
                Some(vec!["codex".to_string(), "app-server".to_string()]),
            ),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &custom_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("helper"));
    }

    #[test]
    fn argv_file_operand_is_not_a_strong_agent_identity() {
        let processes = vec![
            (
                10,
                Some("bash".to_string()),
                args(&["bash", "/tmp/user/.claude"]),
            ),
            (11, Some("pi".to_string()), args(&["pi"])),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &custom_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("pi"));
    }

    #[test]
    fn longest_wrapper_pattern_beats_embedded_builtin_pattern() {
        let signatures = vec![
            AgentSignature {
                name: "codex".to_string(),
                patterns: vec!["codex".to_string()],
            },
            AgentSignature {
                name: "reviewer".to_string(),
                patterns: vec!["codex-review".to_string()],
            },
        ];
        let processes = vec![(
            10,
            Some("codex-review-wrapper".to_string()),
            args(&["codex-review-wrapper"]),
        )];

        let status = detect_agent_in_process_facts_with_signatures(&processes, &signatures);

        assert_eq!(status.agent_name.as_deref(), Some("reviewer"));
    }

    #[test]
    fn leaked_provider_credential_is_weaker_than_pi_identity() {
        let processes = vec![
            (
                10,
                Some("infisical".to_string()),
                args(&[
                    "infisical",
                    "run",
                    "export CLAUDE_CODE_OAUTH_TOKEN; exec $PI_BIN",
                ]),
            ),
            (
                11,
                Some("pi".to_string()),
                args(&["pi", "--session", "pi-session"]),
            ),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &custom_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("pi"));
        assert_eq!(status.session_id.as_deref(), Some("pi-session"));
    }

    #[test]
    fn exact_child_identity_supplies_session_id_after_weak_wrapper_hint() {
        let processes = vec![
            (
                10,
                Some("bash".to_string()),
                args(&["bash", "-lc", "exec codex --resume wrapper-session"]),
            ),
            (
                11,
                Some("codex".to_string()),
                args(&["codex", "--resume", "codex-session"]),
            ),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &custom_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("codex"));
        assert_eq!(status.session_id.as_deref(), Some("codex-session"));
    }

    #[test]
    fn incidental_env_mention_does_not_outrank_a_real_pi_descendant() {
        let processes = vec![
            (
                200,
                Some("env".to_string()),
                args(&["env", "CODEX_TOKEN=redacted", "pi-run"]),
            ),
            (201, Some("pi".to_string()), args(&["pi"])),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &default_agent_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("pi"));
    }

    #[test]
    fn environment_assignment_without_agent_does_not_classify_wrapper() {
        let processes = vec![(
            202,
            Some("env".to_string()),
            args(&["env", "CODEX_TOKEN=redacted", "pi-run"]),
        )];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &default_agent_signatures());

        assert_eq!(status.agent_name, None);
        assert!(!status.running);
    }

    #[test]
    fn exact_identity_anywhere_outranks_a_broad_match_on_the_pane_root() {
        let processes = vec![
            (
                210,
                Some("node".to_string()),
                args(&["node", "/opt/tools/codex-helper.js"]),
            ),
            (
                211,
                Some("claude".to_string()),
                args(&["claude", "--session", "sess-a"]),
            ),
        ];

        let status =
            detect_agent_in_process_facts_with_signatures(&processes, &default_agent_signatures());

        assert_eq!(status.agent_name.as_deref(), Some("claude"));
        assert_eq!(status.session_id.as_deref(), Some("sess-a"));
    }

    #[test]
    fn default_signatures_still_detect_every_supported_agent() {
        for (executable, expected) in [
            ("claude", "claude"),
            ("codex", "codex"),
            ("pi", "pi"),
            ("pii", "pi"),
            ("aider", "aider"),
            ("copilot", "copilot"),
            ("opencode", "opencode"),
            ("cursor", "cursor"),
            ("cline", "cline"),
            ("continue", "continue"),
            ("goose", "goose"),
            ("gemini", "gemini"),
            ("kimi", "kimi"),
        ] {
            let processes = vec![
                (300, Some("zsh".to_string()), args(&["-zsh"])),
                (
                    301,
                    Some(executable.to_string()),
                    args(&[&format!("/usr/local/bin/{executable}")]),
                ),
            ];

            let status = detect_agent_in_process_facts_with_signatures(
                &processes,
                &default_agent_signatures(),
            );

            assert_eq!(
                status.agent_name.as_deref(),
                Some(expected),
                "{executable} should still resolve to {expected}"
            );
        }
    }

    #[test]
    fn configured_signature_outranks_the_builtin_alias_for_the_same_process() {
        let signatures = vec![AgentSignature {
            name: "pi-fork".to_string(),
            patterns: vec!["pi".to_string()],
        }];
        let processes = vec![(400, Some("pi".to_string()), args(&["pi"]))];

        let status = detect_agent_in_process_facts_with_signatures(&processes, &signatures);

        assert_eq!(status.agent_name.as_deref(), Some("pi-fork"));
    }

    #[test]
    fn custom_signature_patterns_still_match_broadly() {
        let signatures = vec![AgentSignature {
            name: "helper".to_string(),
            patterns: vec!["my-custom-agent".to_string()],
        }];
        let processes = vec![(
            410,
            Some("python".to_string()),
            args(&["python", "-m", "vendor.my-custom-agent.cli"]),
        )];

        let status = detect_agent_in_process_facts_with_signatures(&processes, &signatures);

        assert_eq!(status.agent_name.as_deref(), Some("helper"));
    }
}
