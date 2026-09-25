//! Session restore helpers: pane tree reconstruction, tmux metadata polling, and
//! tmux session lifecycle management.

use super::*;

pub(super) fn saved_cwd_to_path(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }
    if cwd.starts_with("file://") {
        let (_host, path) = parse_osc7_uri(cwd);
        return Some(path);
    }
    if cwd.starts_with('/') {
        return Some(cwd.to_string());
    }
    cwd.find('/')
        .map(|idx| cwd[idx..].to_string())
        .or_else(|| Some(cwd.to_string()))
}

pub(super) fn saved_cwd_location(cwd: Option<&str>) -> (Option<String>, Option<String>) {
    let cwd = match cwd.map(str::trim) {
        Some("") | None => return (None, None),
        Some(cwd) => cwd,
    };

    if cwd.starts_with("file://") {
        let (host, path) = parse_osc7_uri(cwd);
        return (Some(path), host);
    }

    if cwd.starts_with('/') || cwd.starts_with('~') {
        return (Some(cwd.to_string()), None);
    }

    if let Some(idx) = cwd.find('/') {
        return (Some(cwd[idx..].to_string()), None);
    }

    (None, None)
}

pub(super) fn display_local_restore_path(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        return path.to_string();
    }

    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }

    if path == home {
        "~".to_string()
    } else if let Some(suffix) = path.strip_prefix(&(home + "/")) {
        format!("~/{}", suffix)
    } else {
        path.to_string()
    }
}

pub(super) fn strip_ssh_target_user(target: &str) -> Option<String> {
    let target = target
        .trim()
        .strip_prefix("ssh://")
        .unwrap_or(target.trim());
    if target.is_empty() {
        return None;
    }
    let host = target
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(target);
    let host = host.trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// Whether an SSH/autossh/mosh option consumes the following argv slot as its
/// value.
///
/// Shared as the single source of truth for both `ssh_command_target_index`
/// (locating the host argument) and `tracking::sanitize_recorded_ssh_connection`
/// (bounding the connection-only probe walk). Keeping one definition prevents the
/// two walks from disagreeing about which arguments are option values.
pub(crate) fn ssh_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-b" | "-B"
            | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-P"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
            | "--bind-server"
            | "--family"
            | "--port"
            | "--predict"
            | "--server"
            | "--ssh"
            | "--user"
    )
}

pub(crate) fn ssh_command_target_index(ssh_command: &[String]) -> Option<usize> {
    // `-M` is an OpenSSH no-argument flag (ControlMaster), but autossh uses the
    // same spelling for its monitor-port argument. Keep that distinction here
    // so `ssh -M host` does not consume `host` as an option value while
    // `autossh -M 0 host` still skips the monitor port.
    let autossh_monitor_option = ssh_command
        .first()
        .and_then(|program| std::path::Path::new(program).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "autossh");
    const VALUE_FLAG_CHARS: &str = "bBcDEeFIiJLlmOoPpQRSWw";
    let mut skip_next = false;
    for (idx, arg) in ssh_command.iter().enumerate().skip(1) {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            continue;
        }
        if ssh_option_takes_value(arg) || (autossh_monitor_option && arg == "-M") {
            skip_next = true;
            continue;
        }
        if let Some(cluster) = arg
            .strip_prefix('-')
            .filter(|value| !value.starts_with('-'))
        {
            if let Some((byte_idx, option)) = cluster
                .char_indices()
                .find(|(_, option)| VALUE_FLAG_CHARS.contains(*option))
            {
                // getopt-compatible clusters may put no-value flags before the
                // value-taking option (`-vp2222` or `-vp 2222`). Only the latter
                // consumes the next argv slot.
                if byte_idx + option.len_utf8() == cluster.len() {
                    skip_next = true;
                }
                continue;
            }
        }
        if arg.starts_with("--") {
            if arg.contains('=') {
                continue;
            }
            if arg == "--no-init" || arg == "--verbose" || arg == "--help" || arg == "--version" {
                continue;
            }
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(idx);
    }
    None
}

pub(super) fn ssh_command_target(ssh_command: &[String]) -> Option<String> {
    let idx = ssh_command_target_index(ssh_command)?;
    Some(ssh_command[idx].clone())
}

pub(super) fn ssh_command_host(ssh_command: &[String]) -> Option<String> {
    strip_ssh_target_user(&ssh_command_target(ssh_command)?)
}

pub(crate) fn ssh_supports_remote_exec(ssh_command: &[String]) -> bool {
    ssh_command
        .first()
        .and_then(|arg| std::path::Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "ssh" | "autossh"))
}

pub(super) fn remote_shell_respawn_command(cwd: &str) -> String {
    format!(
        "cd {} && if [ -n \"$SHELL\" ] && [ -x \"$SHELL\" ]; then exec \"$SHELL\"; else exec /bin/bash; fi",
        crate::mise::shell_quote(cwd),
    )
}

pub(super) fn restored_ssh_command(ssh_command: &[String], cwd: Option<&str>) -> Vec<String> {
    let Some(path) = saved_cwd_to_path(cwd) else {
        return ssh_command.to_vec();
    };
    if !ssh_supports_remote_exec(ssh_command) {
        return ssh_command.to_vec();
    }
    let Some(target_idx) = ssh_command_target_index(ssh_command) else {
        return ssh_command.to_vec();
    };
    if target_idx + 1 != ssh_command.len() {
        return ssh_command.to_vec();
    }

    let mut argv = Vec::with_capacity(ssh_command.len() + 2);
    argv.push(ssh_command[0].clone());
    if !ssh_command
        .iter()
        .skip(1)
        .any(|arg| arg == "-t" || arg == "-tt" || arg == "-T")
    {
        argv.push("-tt".into());
    }
    argv.extend(ssh_command.iter().skip(1).cloned());
    argv.push(remote_shell_respawn_command(&path));
    argv
}

/// Argv for replicating a manual SSH pane into a split or duplicated tab.
///
/// A split replays the focused pane's ssh argv, which lands in the remote home
/// directory. When the pane's OSC 7 location names a remote host and a
/// directory, reuse the session-restore injection so the new pane opens in the
/// same remote cwd. Anything else — no recorded cwd, or a cwd reported against
/// the local machine — replays the argv verbatim, as before.
pub(super) fn remote_split_respawn(
    ssh_command: &[String],
    location: &crate::pane::PaneLocationState,
) -> Vec<String> {
    if !is_remote_host(location.cwd_host.as_deref()) {
        return ssh_command.to_vec();
    }
    restored_ssh_command(ssh_command, location.cwd.as_deref())
}

pub(super) fn restored_leaf_label(
    cwd: Option<&str>,
    ssh_command: Option<&Vec<String>>,
    tmux_session: Option<&str>,
    tmux_host: Option<&str>,
    tmux_identity: Option<&crate::session::SavedTmuxIdentity>,
    agent_session: Option<&crate::session::SavedAgentSession>,
) -> (String, bool) {
    let is_ssh = ssh_command.is_some();
    let (path, cwd_host) = saved_cwd_location(cwd);

    if let Some(session_name) = tmux_session {
        let authority = if tmux_identity.is_some() {
            "Reattach live terminal"
        } else {
            "Reattach unavailable: saved layout lacks exact tmux generation"
        };
        return (
            format!("tmux {session_name} · {authority}"),
            tmux_host.is_some(),
        );
    }

    if is_ssh {
        let host = ssh_command
            .and_then(|argv| ssh_command_host(argv))
            .or(cwd_host);
        let label = match (host, path) {
            (Some(host), Some(path)) => format!("{host}:{path}"),
            (None, Some(path)) => format!("?:{path}"),
            _ => "shell".to_string(),
        };
        return (label, true);
    }

    let label = match path {
        Some(path) => display_local_restore_path(&path),
        None => "shell".to_string(),
    };
    if agent_session.is_some_and(|saved| saved.host_identity.is_none()) {
        return (
            format!("{label} · Resume unavailable: saved layout lacks original host identity"),
            false,
        );
    }
    (label, false)
}

#[derive(Clone, Copy)]
pub(super) struct NormalizedPaneRect {
    pub(super) left: f64,
    pub(super) top: f64,
    pub(super) width: f64,
    pub(super) height: f64,
}

#[derive(Clone)]
pub(super) struct CollectedRestoredPaneHint {
    pub(super) traversal_index: usize,
    pub(super) left: f64,
    pub(super) top: f64,
    pub(super) label: String,
    pub(super) is_ssh: bool,
}

pub(super) fn collect_restored_pane_hints(
    node: &SavedPaneNode,
    rect: NormalizedPaneRect,
    traversal_index: &mut usize,
    leaves: &mut Vec<CollectedRestoredPaneHint>,
) {
    match node {
        SavedPaneNode::Leaf {
            cwd,
            ssh_command,
            tmux_session,
            tmux_host,
            tmux_identity,
            agent_session,
            ..
        } => {
            let current_index = *traversal_index;
            *traversal_index += 1;
            let (label, is_ssh) = restored_leaf_label(
                cwd.as_deref(),
                ssh_command.as_ref(),
                tmux_session.as_deref(),
                tmux_host.as_deref(),
                tmux_identity.as_ref(),
                agent_session.as_ref(),
            );
            leaves.push(CollectedRestoredPaneHint {
                traversal_index: current_index,
                left: rect.left,
                top: rect.top,
                label,
                is_ssh,
            });
        }
        SavedPaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.0, 1.0);
            if direction.eq_ignore_ascii_case("horizontal") {
                let first_height = rect.height * ratio;
                collect_restored_pane_hints(
                    first,
                    NormalizedPaneRect {
                        left: rect.left,
                        top: rect.top,
                        width: rect.width,
                        height: first_height,
                    },
                    traversal_index,
                    leaves,
                );
                collect_restored_pane_hints(
                    second,
                    NormalizedPaneRect {
                        left: rect.left,
                        top: rect.top + first_height,
                        width: rect.width,
                        height: rect.height - first_height,
                    },
                    traversal_index,
                    leaves,
                );
            } else {
                let first_width = rect.width * ratio;
                collect_restored_pane_hints(
                    first,
                    NormalizedPaneRect {
                        left: rect.left,
                        top: rect.top,
                        width: first_width,
                        height: rect.height,
                    },
                    traversal_index,
                    leaves,
                );
                collect_restored_pane_hints(
                    second,
                    NormalizedPaneRect {
                        left: rect.left + first_width,
                        top: rect.top,
                        width: rect.width - first_width,
                        height: rect.height,
                    },
                    traversal_index,
                    leaves,
                );
            }
        }
    }
}

pub(super) fn build_restored_tab_legend(
    saved: &SavedPaneNode,
    tab_id: u32,
) -> Option<RestoredTabLegend> {
    let mut collected = Vec::new();
    let mut traversal_index = 0usize;
    collect_restored_pane_hints(
        saved,
        NormalizedPaneRect {
            left: 0.0,
            top: 0.0,
            width: 1.0,
            height: 1.0,
        },
        &mut traversal_index,
        &mut collected,
    );

    if collected.is_empty() {
        return None;
    }

    let any_ssh = collected.iter().any(|item| item.is_ssh);
    let has_useful_metadata = collected
        .iter()
        .any(|item| item.is_ssh || item.label != "shell");
    if (!any_ssh && collected.len() <= 1) || !has_useful_metadata {
        return None;
    }

    collected.sort_by(|a, b| {
        a.top
            .total_cmp(&b.top)
            .then_with(|| a.left.total_cmp(&b.left))
            .then_with(|| a.traversal_index.cmp(&b.traversal_index))
    });

    Some(RestoredTabLegend {
        tab_id,
        items: collected
            .into_iter()
            .enumerate()
            .map(|(idx, item)| RestoredPaneHint {
                number: idx + 1,
                label: item.label,
                is_ssh: item.is_ssh,
            })
            .collect(),
    })
}

/// Check synchronously whether a tmux session exists.
/// Uses the non-interactive tmux/SSH preflight command so attach attempts fail
/// fast instead of hanging on prompts.
#[cfg(test)]
pub(crate) fn tmux_session_exists(target: &crate::tmux::TmuxTarget, name: &str) -> bool {
    let argv = crate::tmux::has_session_command_batch(target, name);
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a tmux command synchronously and return its stdout.
/// Returns None if the command fails or produces no output.
#[cfg(test)]
pub(super) fn run_tmux_command(argv: &[String]) -> Option<String> {
    if argv.is_empty() {
        return None;
    }
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TmuxKillResult {
    Killed,
    AlreadyMissing,
    TransientFailure(String),
}

pub(crate) fn classify_tmux_kill_result(
    success: bool,
    stderr: &str,
    failure: impl FnOnce() -> String,
) -> TmuxKillResult {
    if success {
        return TmuxKillResult::Killed;
    }
    let stderr_lower = stderr.to_ascii_lowercase();
    if tmux_reports_no_server(&stderr_lower) || stderr_lower.contains("can't find session") {
        TmuxKillResult::AlreadyMissing
    } else {
        TmuxKillResult::TransientFailure(failure())
    }
}

/// Kill a tmux session. Used when close_behavior is Close (the default).
#[cfg(test)]
pub(crate) fn kill_tmux_session(
    target: &crate::tmux::TmuxTarget,
    session_name: &str,
) -> TmuxKillResult {
    let argv = crate::tmux::kill_session_command(target, session_name);
    let output = match std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return TmuxKillResult::TransientFailure(format!(
                "could not execute tmux kill-session: {error}"
            ));
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_tmux_kill_result(output.status.success(), &stderr, || {
        let detail = stderr.trim();
        if detail.is_empty() {
            format!("tmux kill-session exited with {}", output.status)
        } else {
            format!("tmux kill-session failed: {detail}")
        }
    })
}

fn backing_matches(
    candidate: &crate::pane::TmuxBacking,
    backing: &crate::pane::TmuxBacking,
) -> bool {
    candidate.session_name == backing.session_name && candidate.target == backing.target
}

fn workspace_holding_backing<'a>(
    st: &'a AppState,
    backing: &crate::pane::TmuxBacking,
) -> Option<&'a crate::workspace::Workspace> {
    st.workspaces.iter().find(|workspace| {
        workspace.tabs.iter().any(|tab| {
            tab.panes.leaves().into_iter().any(|leaf| {
                leaf.tmux_backing
                    .as_ref()
                    .is_some_and(|candidate| backing_matches(candidate, backing))
            })
        })
    })
}

fn live_pane_holds_backing(st: &AppState, backing: &crate::pane::TmuxBacking) -> bool {
    workspace_holding_backing(st, backing).is_some()
        || st.headless_panes.values().any(|pane| {
            pane.tmux_backing
                .as_ref()
                .is_some_and(|candidate| backing_matches(candidate, backing))
        })
}

fn preserve_tmux_backing_as_detached(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    workspace_hint: Option<&str>,
) -> bool {
    preserve_tmux_backing_as_detached_with_live_policy(state, backing, workspace_hint, false)
}

fn preserve_tmux_backing_as_detached_with_live_policy(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    workspace_hint: Option<&str>,
    allow_live_backing: bool,
) -> bool {
    let mut st = state.borrow_mut();
    if !allow_live_backing && live_pane_holds_backing(&st, backing) {
        st.detached_sessions
            .retain(|session| !session.matches_target(&backing.session_name, &backing.target));
        return false;
    }
    if let Some(existing) = st
        .detached_sessions
        .iter_mut()
        .find(|existing| existing.matches_target(&backing.session_name, &backing.target))
    {
        if let Some(workspace) = workspace_hint {
            existing.workspace = workspace.to_string();
        }
        existing.detached_at = std::time::Instant::now();
        // Preserve completion so a transient kill failure cannot re-notify it.
        return true;
    }
    let headless_tab_id = st.headless_panes.iter().find_map(|((tab_id, _), pane)| {
        pane.tmux_backing
            .as_ref()
            .and_then(|candidate| backing_matches(candidate, backing).then_some(*tab_id))
    });
    let workspace = workspace_hint
        .map(str::to_string)
        .or_else(|| workspace_holding_backing(&st, backing).map(|workspace| workspace.name.clone()))
        .or_else(|| {
            headless_tab_id.and_then(|tab_id| {
                st.workspaces
                    .iter()
                    .find(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
                    .map(|workspace| workspace.name.clone())
            })
        })
        .or_else(|| {
            st.workspaces
                .iter()
                .find(|workspace| workspace.id == st.active_workspace)
                .map(|workspace| workspace.name.clone())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let host = match &backing.target {
        crate::tmux::TmuxTarget::Local => "localhost".to_string(),
        crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
    };
    let last_command = backing.pane_info.value().and_then(|info| {
        (!info.current_command.is_empty() && !crate::tmux::is_shell_command(&info.current_command))
            .then(|| info.current_command.clone())
    });
    let detached = crate::dashboard::DetachedSession {
        session_name: backing.session_name.clone(),
        host,
        workspace,
        target: backing.target.clone(),
        detached_at: std::time::Instant::now(),
        last_command,
        finished: false,
    };
    st.detached_sessions.push(detached);
    true
}

pub(super) fn tmux_kill_result_from_outcome(
    outcome: &crate::tmux::TmuxCommandOutcome,
) -> TmuxKillResult {
    if let Some(error) = outcome.error.as_ref() {
        return TmuxKillResult::TransientFailure(error.clone());
    }
    classify_tmux_kill_result(outcome.success, &outcome.stderr, || {
        let detail = outcome.stderr.trim();
        if detail.is_empty() {
            outcome.status.as_ref().map_or_else(
                || "tmux kill-session failed".to_string(),
                |status| format!("tmux kill-session exited with {status}"),
            )
        } else {
            format!("tmux kill-session failed: {detail}")
        }
    })
}

/// Apply immutable worker results while the source pane/tab still exists.
/// Transient failures are registered in Background first; callers may then
/// finalize GTK/state removal without losing the surviving backing identity.
pub(crate) fn apply_tmux_kill_outcomes_before_removal(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backings: &[crate::pane::TmuxBacking],
    outcomes: &[crate::tmux::TmuxCommandOutcome],
    workspace: &str,
) -> Vec<String> {
    backings
        .iter()
        .enumerate()
        .filter_map(|(index, backing)| {
            let result = outcomes.get(index).map_or_else(
                || {
                    TmuxKillResult::TransientFailure(
                        "tmux cleanup worker returned no result".to_string(),
                    )
                },
                tmux_kill_result_from_outcome,
            );
            match result {
                TmuxKillResult::TransientFailure(error) => {
                    let preserved = preserve_tmux_backing_as_detached_with_live_policy(
                        state,
                        backing,
                        Some(workspace),
                        true,
                    );
                    Some(if preserved {
                        format!(
                            "Could not kill tmux session {}; preserved it in Background: {error}",
                            backing.session_name
                        )
                    } else {
                        format!(
                            "Could not kill tmux session {}: {error}",
                            backing.session_name
                        )
                    })
                }
                result => {
                    apply_tmux_kill_result_in_workspace(state, backing, result, Some(workspace))
                        .err()
                }
            }
        })
        .collect()
}

pub(crate) fn apply_tmux_kill_result(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    result: TmuxKillResult,
) -> Result<(), String> {
    apply_tmux_kill_result_in_workspace(state, backing, result, None)
}

pub(crate) fn apply_tmux_kill_result_in_workspace(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    result: TmuxKillResult,
    workspace_hint: Option<&str>,
) -> Result<(), String> {
    match result {
        TmuxKillResult::Killed | TmuxKillResult::AlreadyMissing => {
            let mut st = state.borrow_mut();
            st.detached_sessions
                .retain(|session| !session.matches_target(&backing.session_name, &backing.target));
            st.invalidate_dashboard_session_snapshot(&backing.target, &backing.session_name);
            Ok(())
        }
        TmuxKillResult::TransientFailure(error) => {
            let preserved = preserve_tmux_backing_as_detached(state, backing, workspace_hint);
            if preserved {
                Err(format!(
                    "Could not kill tmux session {}; preserved it in Background: {error}",
                    backing.session_name
                ))
            } else {
                Err(format!(
                    "Could not kill tmux session {}: {error}",
                    backing.session_name
                ))
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn cleanup_tmux_backing_with_behavior(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    close_behavior: crate::config::TmuxCloseBehavior,
) -> Result<(), String> {
    cleanup_tmux_backing_with_behavior_in_workspace(state, backing, close_behavior, None)
}

#[cfg(test)]
fn cleanup_tmux_backing_with_behavior_in_workspace(
    state: &std::rc::Rc<std::cell::RefCell<AppState>>,
    backing: &crate::pane::TmuxBacking,
    close_behavior: crate::config::TmuxCloseBehavior,
    workspace_hint: Option<&str>,
) -> Result<(), String> {
    match close_behavior {
        crate::config::TmuxCloseBehavior::Close => {
            let result = kill_tmux_session(&backing.target, &backing.session_name);
            apply_tmux_kill_result_in_workspace(state, backing, result, workspace_hint)
        }
        crate::config::TmuxCloseBehavior::Detach => {
            // Detach is a no-op from our side — VTE closing the connection
            // already sends a HUP which detaches the client from tmux
            Ok(())
        }
    }
}

pub(super) fn emit_probe_transition_event(
    state: &mut AppState,
    probe: &str,
    payload: serde_json::Value,
    transition: &ProbeTransition,
    error: Option<&str>,
) {
    if should_record_probe_failure_diagnostic(transition) {
        crate::diagnostics::record_probe_failure(
            "terminal",
            probe,
            format!("probe {probe} is {}", transition.current.label()),
            Some(serde_json::json!({
                "previous_state": transition.previous.label(),
                "state": transition.current.label(),
                "error": error,
                "payload": payload,
            })),
        );
    } else if transition.recovered() {
        crate::diagnostics::record_lifecycle(
            "probe-recovered",
            format!("probe {probe} recovered"),
            Some(serde_json::json!({
                "previous_state": transition.previous.label(),
                "state": transition.current.label(),
                "payload": payload,
            })),
        );
    }

    if !(transition.entered_degraded() || transition.recovered()) {
        return;
    }

    state.event_store.emit(
        "probe_state_changed",
        serde_json::json!({
            "probe": probe,
            "previous_state": transition.previous.label(),
            "state": transition.current.label(),
            "error": error,
            "payload": payload,
        }),
    );
}

pub(super) fn should_record_probe_failure_diagnostic(transition: &ProbeTransition) -> bool {
    transition.entered_degraded()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TmuxCommandFailureBehavior {
    Strict,
    DashboardListSessions,
    Silent,
}

pub(super) fn tmux_reports_no_server(stderr: &str) -> bool {
    stderr.lines().map(str::trim).any(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("no server running on ")
            || (line.starts_with("error connecting to ")
                && line.contains("(no such file or directory)"))
    })
}

/// Query tmux for the current pane info of a tmux-backed pane.
/// Updates the leaf's pane_info in place.
fn remote_tmux_location(
    target: &crate::tmux::TmuxTarget,
    info: &crate::tmux::TmuxPaneInfo,
) -> Option<(String, String)> {
    let crate::tmux::TmuxTarget::Remote { ssh_target } = target else {
        return None;
    };
    let cwd = info.cwd.trim();
    (!cwd.is_empty()).then(|| (cwd.to_string(), ssh_target.clone()))
}

/// Poll all tmux-backed panes and update their pane_info metadata.
pub fn poll_tmux_metadata(state: &Rc<RefCell<AppState>>, tab_list: &gtk::Box) {
    // Collect (tab_id, pane_id, argv) for all tmux-backed panes — lightweight, on main thread
    let targets: Vec<(u32, u32, Vec<String>)> = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .flat_map(|ws| &ws.tabs)
            .flat_map(|tab| {
                tab.panes.leaves().into_iter().filter_map(|leaf| {
                    let backing = leaf.tmux_backing.as_ref()?;
                    let argv =
                        crate::tmux::pane_info_command(&backing.target, &backing.session_name);
                    Some((tab.id, leaf.pane_id, argv))
                })
            })
            .collect()
    };

    if targets.is_empty() {
        return;
    }

    // Run all subprocess calls off the main thread, then update state on main thread
    let state = state.clone();
    let tab_list = tab_list.clone();
    glib::spawn_future_local(async move {
        let results: Vec<(u32, u32, Result<crate::tmux::TmuxPaneInfo, String>)> =
            gio::spawn_blocking(move || {
                targets
                    .into_iter()
                    .map(|(tab_id, pane_id, argv)| {
                        let info = run_tmux_command_sync_result(&argv).and_then(|output| {
                            crate::tmux::parse_pane_info(&output).ok_or_else(|| {
                                "tmux pane probe returned invalid metadata".to_string()
                            })
                        });
                        (tab_id, pane_id, info)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();

        let mut any_changed = false;
        for (tab_id, pane_id, result) in results {
            let mut st = state.borrow_mut();
            let mut event = None;
            if let Some(tab) = st.find_tab_mut(tab_id) {
                if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                    if leaf.tmux_backing.is_some() {
                        let (
                            transition,
                            changed,
                            location_update,
                            session_name,
                            ssh_target,
                            probe_error,
                        ) = {
                            let backing = leaf.tmux_backing.as_mut().expect("checked above");
                            let location_update = result
                                .as_ref()
                                .ok()
                                .and_then(|info| remote_tmux_location(&backing.target, info));
                            let (transition, changed) = match result {
                                Ok(info) => backing.pane_info.record_success_reporting(info),
                                Err(error) => backing.pane_info.record_failure_reporting(error),
                            };
                            (
                                transition,
                                changed,
                                location_update,
                                backing.session_name.clone(),
                                backing.target.ssh_target_string(),
                                backing.pane_info.error.clone(),
                            )
                        };
                        let location_changed =
                            location_update.as_ref().is_some_and(|(cwd, host)| {
                                leaf.location_state.cwd.as_deref() != Some(cwd.as_str())
                                    || leaf.location_state.cwd_host.as_deref()
                                        != Some(host.as_str())
                            });
                        if let Some((cwd, host)) = location_update {
                            if location_changed {
                                leaf.update_location_cache(Some(cwd), Some(host));
                            }
                        }
                        any_changed |= changed || location_changed;
                        event = Some((
                            transition,
                            serde_json::json!({
                                "tab_id": tab_id,
                                "pane_id": pane_id,
                                "session_name": session_name,
                                "ssh_target": ssh_target,
                            }),
                            probe_error,
                        ));
                    }
                }
            }
            if let Some((transition, payload, error)) = event {
                emit_probe_transition_event(
                    &mut st,
                    "tmux-pane-info",
                    payload,
                    &transition,
                    error.as_deref(),
                );
            }
        }

        // Only re-render tab rows when at least one pane's rendered metadata
        // actually changed. At idle the tmux poll returns identical results, so
        // this skips the whole refresh and its per-row work.
        if any_changed {
            crate::sidebar::refresh_all_tab_rows(&tab_list, &state);
        }
    });
}

/// Poll host status for all workspaces that have a host_config_name set.
pub fn poll_host_status(state: &Rc<RefCell<AppState>>) {
    let app_config = crate::config::app_config();

    // Collect (ws_id, ssh_target) pairs — lightweight, on main thread
    let targets: Vec<(u32, String)> = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .filter_map(|ws| {
                let host_name = ws.host_config_name.as_ref()?;
                let host_config = app_config.hosts.iter().find(|h| &h.name == host_name)?;
                let ssh_target = host_config.ssh_target.clone()?;
                Some((ws.id, ssh_target))
            })
            .collect()
    };

    if targets.is_empty() {
        return;
    }

    let state = state.clone();
    glib::spawn_future_local(async move {
        let results: Vec<(u32, Result<crate::host::HostStatus, String>)> =
            gio::spawn_blocking(move || {
                targets
                    .into_iter()
                    .map(|(ws_id, ssh_target)| {
                        let argv = crate::host::probe_status_command(&ssh_target);
                        let status = run_tmux_command_sync_result(&argv).and_then(|output| {
                            crate::host::parse_probe_output(&output).ok_or_else(|| {
                                "host probe returned invalid status output".to_string()
                            })
                        });
                        (ws_id, status)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();

        for (ws_id, result) in results {
            let mut st = state.borrow_mut();
            let mut event = None;
            if let Some(ws) = st.workspaces.iter_mut().find(|w| w.id == ws_id) {
                let transition = match result {
                    Ok(status) => ws.host_status.record_success(status),
                    Err(error) => ws.host_status.record_failure(error),
                };
                event = Some((
                    transition,
                    serde_json::json!({
                        "workspace_id": ws_id,
                        "workspace_name": ws.name,
                        "host_config_name": ws.host_config_name,
                    }),
                    ws.host_status.error.clone(),
                ));
            }
            if let Some((transition, payload, error)) = event {
                emit_probe_transition_event(
                    &mut st,
                    "host-status",
                    payload,
                    &transition,
                    error.as_deref(),
                );
            }
        }
    });
}

/// Run a command synchronously with a timeout. Safe to call from a background thread.
pub(crate) fn run_tmux_command_sync_result(argv: &[String]) -> Result<String, String> {
    run_tmux_command_sync_result_with_behavior(argv, TmuxCommandFailureBehavior::Strict)
}

/// Run a command through the same bounded, pipe-safe path without writing a
/// diagnostic. Callers use this when nonzero exits have domain meaning and must
/// be classified before deciding whether a diagnostic is warranted.
pub(crate) fn run_tmux_command_sync_result_without_diagnostics(
    argv: &[String],
) -> Result<String, String> {
    run_tmux_command_sync_result_with_behavior(argv, TmuxCommandFailureBehavior::Silent)
}

pub(crate) fn run_tmux_list_sessions_command_sync_result(
    argv: &[String],
) -> Result<String, String> {
    run_tmux_command_sync_result_with_behavior(
        argv,
        TmuxCommandFailureBehavior::DashboardListSessions,
    )
}

pub(super) fn run_tmux_command_sync_result_with_behavior(
    argv: &[String],
    failure_behavior: TmuxCommandFailureBehavior,
) -> Result<String, String> {
    if argv.is_empty() {
        if failure_behavior != TmuxCommandFailureBehavior::Silent {
            crate::diagnostics::record_command_failure(
                "terminal",
                "tmux-command",
                "refused to run an empty tmux command argv",
                None,
            );
        }
        return Err("command argv was empty".to_string());
    }
    // Reaped by the try_wait() loop below, including the kill-on-timeout path.
    #[allow(clippy::disallowed_methods)]
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            if failure_behavior != TmuxCommandFailureBehavior::Silent {
                crate::diagnostics::record_command_failure(
                    "terminal",
                    "tmux-command",
                    format!("failed to spawn tmux-related command {}", argv[0]),
                    Some(serde_json::json!({
                        "argv": argv,
                        "error": err.to_string(),
                    })),
                );
            }
            format!("failed to spawn command: {err}")
        })?;

    // Drain stdout/stderr on dedicated threads so a command that writes more than
    // the OS pipe buffer (~64KB) — e.g. a large remote `.plan/tasks.json` fetched
    // over SSH — cannot block on a full pipe and deadlock against the wait loop.
    let stdout_reader = child.stdout.take().map(spawn_pipe_reader);
    let stderr_reader = child.stderr.take().map(spawn_pipe_reader);
    let join_reader = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| {
        reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().map_err(|err| {
            if failure_behavior != TmuxCommandFailureBehavior::Silent {
                crate::diagnostics::record_command_failure(
                    "terminal",
                    "tmux-command",
                    format!("failed while waiting for tmux-related command {}", argv[0]),
                    Some(serde_json::json!({
                        "argv": argv,
                        "error": err.to_string(),
                    })),
                );
            }
            format!("failed to wait for command: {err}")
        })? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // Killing the child closes the pipes, so the readers finish.
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                if failure_behavior != TmuxCommandFailureBehavior::Silent {
                    crate::diagnostics::record_command_failure(
                        "terminal",
                        "tmux-command",
                        format!("tmux-related command {} timed out", argv[0]),
                        Some(serde_json::json!({
                            "argv": argv,
                            "timeout_secs": 10,
                        })),
                    );
                }
                return Err("command timed out after 10s".to_string());
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    };

    let stdout_bytes = join_reader(stdout_reader);
    let stderr_bytes = join_reader(stderr_reader);
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let trimmed_stderr = stderr.trim();
    if !status.success() {
        if matches!(
            failure_behavior,
            TmuxCommandFailureBehavior::DashboardListSessions
        ) && tmux_reports_no_server(trimmed_stderr)
        {
            return Ok(String::new());
        }

        let error = if trimmed_stderr.is_empty() {
            format!("command exited with status {status}")
        } else {
            format!("command exited with status {status}: {trimmed_stderr}")
        };
        if failure_behavior != TmuxCommandFailureBehavior::Silent {
            crate::diagnostics::record_command_failure(
                "terminal",
                "tmux-command",
                format!("tmux-related command {} exited unsuccessfully", argv[0]),
                Some(serde_json::json!({
                    "argv": argv,
                    "status": status.to_string(),
                    "stderr": if trimmed_stderr.is_empty() {
                        None::<String>
                    } else {
                        Some(trimmed_stderr.to_string())
                    },
                })),
            );
        }
        return Err(error);
    }
    Ok(stdout)
}

/// Spawn a thread that drains a child pipe to a byte buffer until EOF.
fn spawn_pipe_reader(
    mut pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

pub(crate) struct DashboardPollOutcome<'a> {
    pub current_commands:
        &'a std::collections::HashMap<crate::dashboard::DetachedSessionCommandKey, String>,
    pub polled_detached_sessions: &'a [(
        crate::dashboard::DetachedSessionCommandKey,
        std::time::Instant,
    )],
    pub live_sessions: &'a crate::dashboard::LiveTmuxSessions,
    pub detached_probe_errors: &'a [(crate::dashboard::DetachedSessionCommandKey, String)],
    pub dashboard_errors: &'a [(Option<crate::tmux::TmuxTarget>, String)],
    pub dashboard_target_count: usize,
    pub detached_target_count: usize,
    pub session_prefix: &'a str,
    pub poll_context: &'a crate::dashboard::DashboardPollContext,
}

pub(crate) fn apply_dashboard_poll_results(state: &mut AppState, poll: DashboardPollOutcome<'_>) {
    let DashboardPollOutcome {
        current_commands,
        polled_detached_sessions,
        live_sessions,
        detached_probe_errors,
        dashboard_errors,
        dashboard_target_count,
        detached_target_count,
        session_prefix,
        poll_context,
    } = poll;
    let poll_is_newer_than_last_completion = state
        .dashboard_poll_tracker
        .poll_is_newer_than_last_completion(poll_context);
    let accepted_live_sessions = live_sessions
        .iter()
        .filter(|(target, _)| {
            state
                .dashboard_poll_tracker
                .result_is_current(poll_context, target)
        })
        .cloned()
        .collect::<Vec<_>>();
    let accepted_target_errors = dashboard_errors
        .iter()
        .filter(|(target, _)| {
            target.as_ref().is_some_and(|target| {
                state
                    .dashboard_poll_tracker
                    .result_is_current(poll_context, target)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let accepted_targets = accepted_live_sessions
        .iter()
        .map(|(target, _)| target.clone())
        .chain(
            accepted_target_errors
                .iter()
                .filter_map(|(target, _)| target.clone()),
        )
        .collect::<std::collections::HashSet<_>>();
    for target in &accepted_targets {
        state
            .dashboard_poll_tracker
            .record_result(poll_context, target);
    }
    let accepted_global_errors = dashboard_errors
        .iter()
        .filter(|(target, _)| target.is_none() && poll_is_newer_than_last_completion)
        .map(|(_, error)| error.clone())
        .collect::<Vec<_>>();
    state.dashboard_poll_tracker.record_completion(poll_context);

    // A successful list-sessions result is authoritative for that exact tmux
    // target, including an empty result produced by tmux's "no server running"
    // response. Remove tracked sessions absent from those successful target
    // snapshots. Targets omitted because SSH/transport probing failed remain
    // untouched, so transient remote failures never become deletion signals.
    let authoritative_sessions: std::collections::HashMap<
        crate::tmux::TmuxTarget,
        std::collections::HashSet<String>,
    > = accepted_live_sessions
        .iter()
        .map(|(target, sessions)| {
            (
                target.clone(),
                sessions
                    .iter()
                    .map(|(name, _, _, _)| name.clone())
                    .collect(),
            )
        })
        .collect();
    let polled_detached_sessions: std::collections::HashMap<_, _> =
        polled_detached_sessions.iter().cloned().collect();
    let removed: std::collections::HashSet<crate::dashboard::DetachedSessionCommandKey> = state
        .detached_sessions
        .iter()
        .filter_map(|session| {
            let key = crate::dashboard::detached_session_command_key(
                &session.target,
                &session.session_name,
            );
            (polled_detached_sessions
                .get(&key)
                .is_some_and(|detached_at| *detached_at == session.detached_at)
                && authoritative_sessions
                    .get(&session.target)
                    .is_some_and(|names| !names.contains(&session.session_name)))
            .then_some(key)
        })
        .collect();
    if !removed.is_empty() {
        state.detached_sessions.retain(|session| {
            !removed.contains(&crate::dashboard::detached_session_command_key(
                &session.target,
                &session.session_name,
            ))
        });
    }

    // A failed pane command probe is expected after an authoritative target
    // snapshot confirms that exact detached session is gone. Suppress only
    // those errors and results for detached generations that changed while the
    // async poll was running; all current pane and target failures stay visible.
    let current_detached_generations: std::collections::HashMap<_, _> = state
        .detached_sessions
        .iter()
        .map(|session| {
            (
                crate::dashboard::detached_session_command_key(
                    &session.target,
                    &session.session_name,
                ),
                session.detached_at,
            )
        })
        .collect();
    let current_commands = current_commands
        .iter()
        .filter(|(key, _)| {
            polled_detached_sessions
                .get(*key)
                .zip(current_detached_generations.get(*key))
                .is_some_and(|(polled, current)| polled == current)
        })
        .map(|(key, command)| (key.clone(), command.clone()))
        .collect();
    crate::dashboard::check_and_notify_finished(&mut state.detached_sessions, &current_commands);

    let mut errors = detached_probe_errors
        .iter()
        .filter(|(key, _)| {
            accepted_targets.contains(&key.0)
                && polled_detached_sessions
                    .get(key)
                    .zip(current_detached_generations.get(key))
                    .is_some_and(|(polled, current)| polled == current)
        })
        .map(|(_, error)| error.clone())
        .collect::<Vec<_>>();
    errors.extend(
        accepted_target_errors
            .iter()
            .map(|(_, error)| error.clone()),
    );
    errors.extend(accepted_global_errors);

    let next_state = crate::dashboard::aggregate_dashboard_state(
        &state.workspaces,
        &state.detached_sessions,
        &accepted_live_sessions,
        &std::collections::HashMap::new(),
        session_prefix,
    );
    let all_target_results_accepted = accepted_targets.len() == dashboard_target_count;
    if errors.is_empty() && all_target_results_accepted {
        let transition = state
            .dashboard_state
            .apply_success(next_state.sessions, next_state.hosts);
        let error = state.dashboard_state.probe.error.clone();
        emit_probe_transition_event(
            state,
            "dashboard-state",
            serde_json::json!({
                "dashboard_targets": dashboard_target_count,
                "detached_targets": detached_target_count,
            }),
            &transition,
            error.as_deref(),
        );
    } else {
        // Reconcile successful targets even when another target failed. Rows and
        // hosts for failed targets retain their last-known snapshot, while each
        // authoritative target is replaced wholesale so host session counts and
        // empty-target state cannot remain stale.
        let authoritative_targets: std::collections::HashSet<_> = accepted_live_sessions
            .iter()
            .map(|(target, _)| target.clone())
            .collect();
        let authoritative_hosts: std::collections::HashSet<_> = authoritative_targets
            .iter()
            .map(|target| match target {
                crate::tmux::TmuxTarget::Local => "localhost".to_string(),
                crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
            })
            .collect();
        state
            .dashboard_state
            .sessions
            .retain(|session| !authoritative_targets.contains(&session.target));
        state.dashboard_state.sessions.extend(
            next_state
                .sessions
                .into_iter()
                .filter(|session| authoritative_targets.contains(&session.target)),
        );
        state
            .dashboard_state
            .hosts
            .retain(|host| !authoritative_hosts.contains(&host.name));
        state.dashboard_state.hosts.extend(
            next_state
                .hosts
                .into_iter()
                .filter(|host| authoritative_hosts.contains(&host.name)),
        );

        if !errors.is_empty() {
            let error_message = format!("dashboard probe incomplete: {}", errors.join("; "));
            let transition = state.dashboard_state.apply_failure(error_message);
            let error = state.dashboard_state.probe.error.clone();
            emit_probe_transition_event(
                state,
                "dashboard-state",
                serde_json::json!({
                    "dashboard_targets": dashboard_target_count,
                    "detached_targets": detached_target_count,
                    "errors": &errors,
                }),
                &transition,
                error.as_deref(),
            );
        }
    }
}

/// Resolve the spawn descriptor for a single restored leaf: its working
/// directory, the command to run (SSH replay or tmux attach when applicable),
/// and any tmux backing to record on the live leaf. Shared by the GTK pane
/// builder ([`build_restored_pane_tree`]) and the GTK-free planner
/// ([`plan_restored_spawns`]) so cwd/ssh/tmux resolution stays single-sourced.
fn restored_leaf_spawn(
    cwd: Option<&str>,
    ssh_command: Option<&Vec<String>>,
    tmux_session: Option<&str>,
    tmux_host: Option<&str>,
    tmux_identity: Option<&crate::session::SavedTmuxIdentity>,
    agent_session: Option<&crate::session::SavedAgentSession>,
    auto_resume_agents: bool,
) -> (
    Option<String>,
    Option<Vec<String>>,
    Option<crate::pane::TmuxBacking>,
    Option<AgentResumeOffer>,
) {
    // A saved name is display metadata only. Reattach requires the exact tmux
    // server/session generation observed while the layout was saved.
    if let Some(session_name) = tmux_session {
        let target = match tmux_host {
            Some(ssh) => crate::tmux::TmuxTarget::Remote {
                ssh_target: ssh.to_string(),
            },
            None => crate::tmux::TmuxTarget::Local,
        };
        let Some(identity) = tmux_identity else {
            return (saved_cwd_to_path(cwd), None, None, None);
        };
        let Some(argv) = crate::tmux::exact_attach_command(
            &target,
            session_name,
            &identity.session_id,
            identity.session_created,
            &identity.continuity_id,
        ) else {
            return (saved_cwd_to_path(cwd), None, None, None);
        };
        let backing = crate::pane::TmuxBacking {
            session_name: session_name.to_string(),
            target,
            pane_info: ProbeSnapshot::default(),
        };
        // For tmux, cwd is handled by tmux itself.
        (None, Some(argv), Some(backing), None)
    } else {
        let spawn_cwd = saved_cwd_to_path(cwd);
        let ssh_spawn = ssh_command.map(|argv| restored_ssh_command(argv, cwd));
        if ssh_spawn.is_some() {
            return (spawn_cwd, ssh_spawn, None, None);
        }
        let agent_resume = agent_session.and_then(|saved| {
            let cwd = spawn_cwd.as_deref()?;
            let reference = agent_session_core::StableRef::new(
                &saved.agent_name,
                saved.host_identity.as_deref()?,
                &saved.session_id,
            );
            if reference.host_identity.is_empty() {
                return None;
            }
            let selector = crate::saved_agent_resume_handoff(reference).ok()?;
            Some(AgentResumeOffer {
                agent_name: saved.agent_name.clone(),
                action: agent_session_core::ActionPlan {
                    kind: agent_session_core::ActionKind::Resume,
                    transport: agent_session_core::Transport::Local,
                    // VTE may resolve `/proc/self/exe` in its spawn helper.
                    // Bind the exact parent process instead so reload cannot
                    // substitute a newer app artifact before validation.
                    program: format!("/proc/{}/exe", std::process::id()),
                    argv: vec!["--resume-saved-agent".into(), selector],
                    cwd: cwd.into(),
                    remote: None,
                    confirmation: agent_session_core::Confirmation::Required,
                    attach: None,
                },
                command: crate::agent_sessions::build_resume_command(
                    &saved.agent_name,
                    cwd,
                    &saved.session_id,
                ),
            })
        });
        let spawn_cmd = auto_resume_agents
            .then_some(agent_resume.as_ref())
            .flatten()
            .map(|offer| offer.argv());
        (spawn_cwd, spawn_cmd, None, agent_resume)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoResumeAgents(bool);

impl AutoResumeAgents {
    pub fn enabled(self) -> bool {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRestoreResumePolicy;

impl SessionRestoreResumePolicy {
    pub fn eager(configured_auto_resume: bool, resume_after_reload: bool) -> AutoResumeAgents {
        AutoResumeAgents(configured_auto_resume || resume_after_reload)
    }

    pub fn lazy(configured_auto_resume: bool) -> AutoResumeAgents {
        AutoResumeAgents(configured_auto_resume)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumeOffer {
    pub agent_name: String,
    /// Display/copy metadata only. Never an execution input.
    pub command: String,
    pub action: agent_session_core::ActionPlan,
}

impl AgentResumeOffer {
    fn argv(&self) -> Vec<String> {
        std::iter::once(self.action.program.clone())
            .chain(self.action.argv.clone())
            .collect()
    }

    /// A restored idle pane already owns a shell. Encode structured words at
    /// that boundary; never read the compatibility command. Automatic restore
    /// bypasses the shell entirely and passes `argv()` to the spawn seam.
    pub(crate) fn shell_input(&self) -> String {
        let escape = agent_session_core::legacy::shell_escape;
        let words = self
            .argv()
            .iter()
            .map(|word| escape(word))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "cd {} && {}",
            escape(&self.action.cwd.to_string_lossy()),
            words
        )
    }
}

/// A single leaf's planned spawn, produced without touching GTK. Mirrors what
/// [`build_restored_pane_tree`] pushes into its spawn list, so a lazily restored
/// tab spawns exactly the same processes on first activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRestoreSpawn {
    pub pane_id: u32,
    pub cwd: Option<String>,
    pub cwd_host: Option<String>,
    pub remote_shell: bool,
    pub spawn_cwd: Option<String>,
    pub spawn_cmd: Option<Vec<String>>,
    pub tmux_session: Option<String>,
    pub tmux_host: Option<String>,
    pub current_task: Option<crate::task_binding::PaneTaskBinding>,
    pub agent_resume: Option<AgentResumeOffer>,
}

/// Walk a saved pane tree in the same DFS pre-order as
/// [`build_restored_pane_tree`], assigning pane ids and resolving each leaf's
/// spawn descriptor. GTK-free, so it is unit-testable in headless runs.
pub fn plan_restored_spawns(
    saved: &SavedPaneNode,
    auto_resume_agents: bool,
) -> Vec<PlannedRestoreSpawn> {
    let mut next_pane_id = 0u32;
    let mut out = Vec::new();
    plan_restored_spawns_inner(saved, auto_resume_agents, &mut next_pane_id, &mut out);
    out
}

fn plan_restored_spawns_inner(
    saved: &SavedPaneNode,
    auto_resume_agents: bool,
    next_pane_id: &mut u32,
    out: &mut Vec<PlannedRestoreSpawn>,
) {
    match saved {
        SavedPaneNode::Leaf {
            cwd,
            ssh_command,
            tmux_session,
            tmux_host,
            tmux_identity,
            current_task,
            agent_session,
            ..
        } => {
            let pane_id = *next_pane_id;
            *next_pane_id += 1;
            let (spawn_cwd, spawn_cmd, backing, agent_resume) = restored_leaf_spawn(
                cwd.as_deref(),
                ssh_command.as_ref(),
                tmux_session.as_deref(),
                tmux_host.as_deref(),
                tmux_identity.as_ref(),
                agent_session.as_ref(),
                auto_resume_agents,
            );
            let (saved_cwd, saved_cwd_host) = saved_cwd_location(cwd.as_deref());
            out.push(PlannedRestoreSpawn {
                pane_id,
                cwd: saved_cwd,
                cwd_host: saved_cwd_host,
                remote_shell: ssh_command.is_some() || tmux_host.is_some(),
                spawn_cwd,
                spawn_cmd,
                tmux_session: backing.as_ref().map(|b| b.session_name.clone()),
                tmux_host: tmux_host.clone(),
                current_task: current_task.clone(),
                agent_resume,
            });
        }
        SavedPaneNode::Split { first, second, .. } => {
            plan_restored_spawns_inner(first, auto_resume_agents, next_pane_id, out);
            plan_restored_spawns_inner(second, auto_resume_agents, next_pane_id, out);
        }
    }
}

pub(super) fn build_restored_pane_tree(
    saved: &SavedPaneNode,
    cfg: &GhosttyConfig,
    next_pane_id: &mut u32,
    spawns: &mut Vec<RestoredPaneSpawn>,
    auto_resume_agents: bool,
) -> PaneNode {
    match saved {
        SavedPaneNode::Leaf {
            work_origin,
            cwd,
            ssh_command,
            tmux_session,
            tmux_host,
            tmux_identity,
            current_task,
            agent_session,
        } => {
            let pane_id = *next_pane_id;
            *next_pane_id += 1;
            let (terminal, container) = build_terminal(cfg);

            let (spawn_cwd, spawn_cmd, backing, agent_resume) = restored_leaf_spawn(
                cwd.as_deref(),
                ssh_command.as_ref(),
                tmux_session.as_deref(),
                tmux_host.as_deref(),
                tmux_identity.as_ref(),
                agent_session.as_ref(),
                auto_resume_agents,
            );

            spawns.push((pane_id, terminal.clone(), spawn_cwd, spawn_cmd));
            let mut leaf = build_pane_leaf(pane_id, &terminal, &container, None);
            leaf.work_origin = work_origin
                .clone()
                .unwrap_or_else(crate::pane::new_pane_work_origin);
            leaf.tmux_backing = backing;
            leaf.current_task = current_task.clone();
            leaf.restored_agent_session = agent_session.clone();
            leaf.agent_resume = agent_resume;
            PaneNode::Leaf(leaf)
        }
        SavedPaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let split_direction = if direction.eq_ignore_ascii_case("horizontal") {
                crate::pane::SplitDirection::Horizontal
            } else {
                crate::pane::SplitDirection::Vertical
            };
            let orientation = match split_direction {
                crate::pane::SplitDirection::Vertical => gtk::Orientation::Horizontal,
                crate::pane::SplitDirection::Horizontal => gtk::Orientation::Vertical,
            };
            let first_node =
                build_restored_pane_tree(first, cfg, next_pane_id, spawns, auto_resume_agents);
            let second_node =
                build_restored_pane_tree(second, cfg, next_pane_id, spawns, auto_resume_agents);
            let paned = gtk::Paned::new(orientation);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            paned.set_start_child(Some(
                &first_node
                    .root_widget()
                    .expect("a restored split child always has a root widget"),
            ));
            paned.set_end_child(Some(
                &second_node
                    .root_widget()
                    .expect("a restored split child always has a root widget"),
            ));
            schedule_paned_ratio(&paned, orientation, *ratio);
            PaneNode::Split {
                direction: split_direction,
                first: Box::new(first_node),
                second: Box::new(second_node),
                widget: paned,
            }
        }
    }
}

#[cfg(test)]
mod sync_command_tests {
    use super::{remote_tmux_location, run_tmux_command_sync_result};

    #[test]
    fn remote_tmux_probe_supplies_authoritative_location() {
        let target = crate::tmux::TmuxTarget::Remote {
            ssh_target: "builder@remote".to_string(),
        };
        let info = crate::tmux::TmuxPaneInfo {
            current_command: "zsh".to_string(),
            cwd: "/srv/project".to_string(),
            pid: 42,
            width: 120,
            height: 40,
            session_id: "$1".into(),
            session_created: 1,
            continuity_id: Some("11".repeat(16)),
        };

        assert_eq!(
            remote_tmux_location(&target, &info),
            Some(("/srv/project".to_string(), "builder@remote".to_string()))
        );
        assert_eq!(
            remote_tmux_location(&crate::tmux::TmuxTarget::Local, &info),
            None
        );
    }

    #[test]
    fn run_tmux_command_sync_result_captures_output_larger_than_pipe_buffer() {
        // A command writing well past the OS pipe buffer (~64KB) must be captured
        // in full rather than deadlocking against the wait loop. Regression for
        // remote `.plan/tasks.json` fetches whose JSON exceeds the pipe buffer.
        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            "for i in $(seq 1 4000); do printf '%050d\\n' \"$i\"; done".to_string(),
        ];
        let out =
            run_tmux_command_sync_result(&argv).expect("large output should be captured, not hang");
        // 50 digits + newline per line, 4000 lines (~200KB, far past the buffer).
        assert_eq!(out.len(), 4000 * 51);
        assert!(out.lines().next().unwrap().ends_with("0001"));
    }
}

#[cfg(test)]
mod agent_resume_tests {
    use super::{plan_restored_spawns, restored_leaf_label, SessionRestoreResumePolicy};
    use crate::session::{SavedAgentSession, SavedAgentSessionSource, SavedPaneNode};

    fn leaf(agent_session: Option<SavedAgentSession>) -> SavedPaneNode {
        SavedPaneNode::Leaf {
            work_origin: None,
            cwd: Some("/repo with spaces".into()),
            ssh_command: None,
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session,
        }
    }

    fn codex_session() -> SavedAgentSession {
        SavedAgentSession {
            agent_name: "codex".into(),
            session_id: "session-123".into(),
            host_identity: Some("build.ts".into()),
            source: SavedAgentSessionSource::Argv,
        }
    }

    #[test]
    fn default_restore_offers_exact_resume_command_without_executing_it() {
        let plan = plan_restored_spawns(&leaf(Some(codex_session())), false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].spawn_cwd.as_deref(), Some("/repo with spaces"));
        assert!(plan[0].spawn_cmd.is_none());
        let offer = plan[0]
            .agent_resume
            .as_ref()
            .expect("resume should be offered");
        assert_eq!(offer.agent_name, "codex");
        assert_eq!(
            offer.command,
            crate::agent_sessions::build_resume_command(
                "codex",
                "/repo with spaces",
                "session-123"
            )
        );
    }

    #[test]
    fn auto_resume_executes_structured_argv() {
        let plan = plan_restored_spawns(&leaf(Some(codex_session())), true);
        let offer = plan[0]
            .agent_resume
            .as_ref()
            .expect("resume should remain available for labeling");
        let argv = plan[0]
            .spawn_cmd
            .as_ref()
            .expect("auto resume should supply a command");
        assert_eq!(argv[0], format!("/proc/{}/exe", std::process::id()));
        assert_eq!(argv[1], "--resume-saved-agent");
        let handoff: serde_json::Value =
            serde_json::from_str(&argv[2]).expect("resume handoff must be typed JSON");
        assert_eq!(handoff["reference"]["provider_id"], "codex");
        assert_eq!(handoff["reference"]["session_id"], "session-123");
        assert!(!handoff["reference"]["host_identity"]
            .as_str()
            .unwrap()
            .is_empty());
        assert_eq!(
            handoff["expected_app_build"],
            serde_json::to_value(crate::runtime_identity::build_identity()).unwrap()
        );
        assert!(!argv.contains(&offer.command));
        assert!(!argv.iter().any(|word| word == "/proc/self/exe"));
    }

    #[test]
    fn manual_resume_ignores_compatibility_command() {
        let plan = plan_restored_spawns(&leaf(Some(codex_session())), false);
        let mut offer = plan[0].agent_resume.clone().unwrap();
        offer.command = "must never execute this metadata".into();
        offer.action.argv = vec!["--resume-saved-agent".into(), "id with 'quotes'".into()];
        let expected_program = format!("/proc/{}/exe", std::process::id());
        assert_eq!(
            offer.argv(),
            [
                expected_program.clone(),
                "--resume-saved-agent".into(),
                "id with 'quotes'".into()
            ]
        );
        assert_eq!(
            offer.shell_input(),
            format!(
                "cd '/repo with spaces' && {expected_program} --resume-saved-agent 'id with '\"'\"'quotes'\"'\"''"
            )
        );
    }

    #[test]
    fn reload_resume_intent_applies_to_eager_restore_only_when_config_is_disabled() {
        let eager = plan_restored_spawns(
            &leaf(Some(codex_session())),
            SessionRestoreResumePolicy::eager(false, true).enabled(),
        );
        let lazy = plan_restored_spawns(
            &leaf(Some(codex_session())),
            SessionRestoreResumePolicy::lazy(false).enabled(),
        );

        assert!(eager[0].spawn_cmd.is_some());
        assert!(lazy[0].spawn_cmd.is_none());
        assert!(lazy[0].agent_resume.is_some());
    }

    #[test]
    fn leaf_without_agent_session_keeps_bare_shell_behavior() {
        let plan = plan_restored_spawns(&leaf(None), false);
        assert_eq!(plan[0].spawn_cwd.as_deref(), Some("/repo with spaces"));
        assert!(plan[0].spawn_cmd.is_none());
        assert!(plan[0].agent_resume.is_none());
    }

    #[test]
    fn tmux_restore_plan_ignores_saved_agent_session() {
        let tmux_leaf = |agent_session| SavedPaneNode::Leaf {
            work_origin: None,
            cwd: Some("/repo".into()),
            ssh_command: None,
            tmux_session: Some("taarof--repo--0".into()),
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session,
        };
        let before = plan_restored_spawns(&tmux_leaf(None), false);
        let after = plan_restored_spawns(&tmux_leaf(Some(codex_session())), false);
        assert_eq!(after, before);
        assert!(after[0].agent_resume.is_none());
    }

    #[test]
    fn legacy_agent_without_original_host_cannot_rebind_on_this_machine() {
        let mut legacy = codex_session();
        legacy.host_identity = None;
        let plan = plan_restored_spawns(&leaf(Some(legacy.clone())), true);
        assert!(plan[0].spawn_cmd.is_none());
        assert!(plan[0].agent_resume.is_none());
        let (label, _) = restored_leaf_label(
            Some("/repo with spaces"),
            None,
            None,
            None,
            None,
            Some(&legacy),
        );
        assert!(label.contains("Resume unavailable: saved layout lacks original host identity"));
    }

    #[test]
    fn legacy_tmux_name_surfaces_exact_generation_failure() {
        let (label, _) =
            restored_leaf_label(Some("/repo"), None, Some("same-name"), None, None, None);
        assert_eq!(
            label,
            "tmux same-name · Reattach unavailable: saved layout lacks exact tmux generation"
        );
        let plan = plan_restored_spawns(
            &SavedPaneNode::Leaf {
                work_origin: None,
                cwd: Some("/repo".into()),
                ssh_command: None,
                tmux_session: Some("same-name".into()),
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            },
            false,
        );
        assert!(plan[0].spawn_cmd.is_none());
        assert!(plan[0].tmux_session.is_none());
    }
}

#[cfg(test)]
mod remote_split_tests {
    use super::{remote_shell_respawn_command, remote_split_respawn, restored_ssh_command};
    use crate::pane::PaneLocationState;

    fn ssh_argv() -> Vec<String> {
        vec!["ssh".to_string(), "builder@devbox".to_string()]
    }

    #[test]
    fn test_remote_split_respawn_with_known_cwd_appends_cd_and_shell_exec() {
        let location = PaneLocationState {
            cwd: Some("/srv/api".to_string()),
            cwd_host: Some("devbox".to_string()),
            updated_at_unix_ms: None,
        };

        let argv = remote_split_respawn(&ssh_argv(), &location);

        assert_eq!(
            argv,
            vec![
                "ssh".to_string(),
                "-tt".to_string(),
                "builder@devbox".to_string(),
                remote_shell_respawn_command("/srv/api"),
            ]
        );
        // The split path must produce exactly the restore path's shape.
        assert_eq!(argv, restored_ssh_command(&ssh_argv(), Some("/srv/api")));
    }

    #[test]
    fn test_remote_split_respawn_without_cwd_replays_argv_verbatim() {
        let location = PaneLocationState {
            cwd: None,
            cwd_host: Some("devbox".to_string()),
            updated_at_unix_ms: None,
        };

        assert_eq!(remote_split_respawn(&ssh_argv(), &location), ssh_argv());
        assert_eq!(
            remote_split_respawn(&ssh_argv(), &PaneLocationState::default()),
            ssh_argv()
        );
    }

    #[test]
    fn test_remote_split_respawn_ignores_local_cwd_host() {
        // OSC 7 reported the local machine: the pane's cwd is a local path, so
        // injecting a remote `cd` would land the split in a directory that does
        // not exist on the far side.
        let location = PaneLocationState {
            cwd: Some("/tmp/user/project".to_string()),
            cwd_host: Some("localhost".to_string()),
            updated_at_unix_ms: None,
        };

        assert_eq!(remote_split_respawn(&ssh_argv(), &location), ssh_argv());

        let no_host = PaneLocationState {
            cwd: Some("/tmp/user/project".to_string()),
            cwd_host: None,
            updated_at_unix_ms: None,
        };
        assert_eq!(remote_split_respawn(&ssh_argv(), &no_host), ssh_argv());
    }
}
