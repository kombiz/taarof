use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Shared cadence for background tmux pane metadata and dashboard polling.
pub(crate) const TMUX_METADATA_POLL_SECONDS: u32 = 5;
/// Allow one scheduled refresh plus bounded scheduling/worker overlap. This is
/// metadata freshness only; process evidence keeps its independent short TTL.
pub(crate) const TMUX_METADATA_TTL_MS: u64 = TMUX_METADATA_POLL_SECONDS as u64 * 2_000;

/// Overall deadline for one socket/HTTP tmux control mutation. The HTTP bridge
/// derives its synchronous wait from this value so it cannot time out while a
/// compliant tmux worker may still succeed.
pub(crate) const TMUX_CONTROL_DEADLINE: Duration = Duration::from_secs(10);

/// Overall deadline for multi-backing close cleanup.
pub(crate) const TMUX_CLOSE_DEADLINE: Duration = Duration::from_secs(12);

/// How to reach the tmux server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TmuxTarget {
    Local,
    Remote { ssh_target: String },
}

impl TmuxTarget {
    pub fn ssh_target_string(&self) -> Option<String> {
        match self {
            TmuxTarget::Local => None,
            TmuxTarget::Remote { ssh_target } => Some(ssh_target.clone()),
        }
    }
}

/// Whether taarof styles the tmux sessions it creates itself.
///
/// Only sessions taarof creates are ever styled; attaching to a session someone
/// else made ([`attach_command`]) is untouched by this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TmuxSessionStyle {
    /// Set nothing. The session inherits the user's tmux defaults and
    /// `~/.tmux.conf` exactly as before this option existed.
    #[default]
    Inherit,
    /// Apply [`PLAIN_SESSION_OPTIONS`] to the session taarof just created, so a
    /// taarof pane looks like a plain terminal rather than a tmux window.
    Plain,
}

/// Session-scoped options applied for [`TmuxSessionStyle::Plain`].
///
/// Session scope (`set-option -t <session>`, never `-g`) is the point: the
/// user's global options and `~/.tmux.conf` stay untouched, and any tmux
/// session taarof did not create keeps its own look.
const PLAIN_SESSION_OPTIONS: [(&str, &str); 2] = [("status", "off"), ("mouse", "on")];

/// tmux's own command separator. Passed as its own argv element, so tmux reads
/// it as "next command" rather than as part of the previous one. Shells spell
/// this `\;`; an exec'd argv spells it `;`, and [`quote_remote_shell_arg`]
/// quotes it back into a literal for the remote shell.
const TMUX_COMMAND_SEPARATOR: &str = ";";
const CONTINUITY_OPTION: &str = "@taarof-continuity-id";
pub const EXACT_ATTACH_UNAVAILABLE_REASON: &str =
    "Reattach unavailable: the exact saved tmux target no longer exists.";

const PANE_INFO_SEPARATOR: &str = "__TAAROF_PANE_INFO_V1__";
const LEGACY_PANE_INFO_SEPARATOR: &str = "\u{1f}";

/// Generate a tmux session name from prefix, workspace, tab_id, and pane_id.
/// Format: `{prefix}--{workspace}--t{tab_id}--{pane_id}`
/// Uses tab_id (globally unique) instead of tab name to avoid collisions
/// when multiple tabs share the same name.
/// Sanitizes dots, colons, spaces → underscores. Truncates to 128 chars max.
pub fn session_name(prefix: &str, workspace: &str, tab_id: u32, pane_id: u32) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                '.' | ':' | ' ' => '_',
                other => other,
            })
            .collect()
    };

    let raw = format!(
        "{}--{}--t{}--{}",
        sanitize(prefix),
        sanitize(workspace),
        tab_id,
        pane_id
    );

    if raw.len() > 128 {
        // Truncate at a char boundary to avoid panic on multi-byte UTF-8
        let end = raw
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= 128)
            .last()
            .unwrap_or(0);
        raw[..end].to_string()
    } else {
        raw
    }
}

/// Wrap a tmux command for a given target.
/// For Remote targets, prefixes with `ssh -t <ssh_target>`.
/// Used for interactive terminal commands (needs PTY allocation via -t).
fn wrap_for_target(target: &TmuxTarget, tmux_args: Vec<String>) -> Vec<String> {
    match target {
        TmuxTarget::Local => tmux_args,
        TmuxTarget::Remote { ssh_target } => {
            let mut cmd = vec!["ssh".to_string(), "-t".to_string(), ssh_target.clone()];
            cmd.extend(tmux_args.into_iter().map(quote_remote_shell_arg));
            cmd
        }
    }
}

/// Wrap a tmux command for non-interactive (polling) use.
/// Uses BatchMode and ConnectTimeout to avoid auth prompts and hangs.
pub(crate) fn wrap_for_target_noninteractive(
    target: &TmuxTarget,
    tmux_args: Vec<String>,
) -> Vec<String> {
    match target {
        TmuxTarget::Local => tmux_args,
        TmuxTarget::Remote { ssh_target } => {
            let mut cmd = vec![
                "ssh".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-o".to_string(),
                "ConnectTimeout=5".to_string(),
                ssh_target.clone(),
            ];
            cmd.extend(tmux_args.into_iter().map(quote_remote_shell_arg));
            cmd
        }
    }
}

fn quote_remote_shell_arg(arg: String) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }

    let is_shell_safe = arg
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'));
    if is_shell_safe {
        return arg;
    }

    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// `tmux new-session -As {name} [-c {cwd}]`, followed by an owned continuity
/// generation and, for
/// [`TmuxSessionStyle::Plain`] by `; set-option -t {name} <option> <value>` per
/// [`PLAIN_SESSION_OPTIONS`].
///
/// The styling rides in the same command sequence as the create/attach so one
/// child does both — no second process, no window in which an unstyled session
/// is visible, and the existing local/remote wrapping applies unchanged.
/// [`TmuxSessionStyle::Inherit`] is the default and adds no visual styling.
pub fn create_attach_command(
    target: &TmuxTarget,
    name: &str,
    cwd: Option<&str>,
    style: TmuxSessionStyle,
) -> Vec<String> {
    let mut bytes = [0_u8; 16];
    let continuity_id = getrandom::getrandom(&mut bytes).ok().map(|()| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });
    create_attach_command_with_continuity_id(target, name, cwd, style, continuity_id.as_deref())
}

fn create_attach_command_with_continuity_id(
    target: &TmuxTarget,
    name: &str,
    cwd: Option<&str>,
    style: TmuxSessionStyle,
    continuity_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "tmux".to_string(),
        "new-session".to_string(),
        "-As".to_string(),
        name.to_string(),
    ];
    if let Some(dir) = cwd {
        args.push("-c".to_string());
        args.push(dir.to_string());
    }
    if let Some(continuity_id) = continuity_id {
        let escaped_name = agent_session_core::legacy::shell_escape(name);
        args.extend([
            TMUX_COMMAND_SEPARATOR.to_string(),
            "if-shell".to_string(),
            "-t".to_string(),
            name.to_string(),
            "-F".to_string(),
            format!("#{{==:#{{{CONTINUITY_OPTION}}},}}"),
            format!("set-option -t {escaped_name} {CONTINUITY_OPTION} {continuity_id}"),
            String::new(),
        ]);
    }
    if style == TmuxSessionStyle::Plain {
        for (option, value) in PLAIN_SESSION_OPTIONS {
            args.extend([
                TMUX_COMMAND_SEPARATOR.to_string(),
                "set-option".to_string(),
                "-t".to_string(),
                name.to_string(),
                option.to_string(),
                value.to_string(),
            ]);
        }
    }
    wrap_for_target(target, args)
}

/// `tmux kill-session -t {name}`
/// Uses non-interactive SSH (BatchMode) since this is a background command.
pub fn kill_session_command(target: &TmuxTarget, name: &str) -> Vec<String> {
    let args = vec![
        "tmux".to_string(),
        "kill-session".to_string(),
        "-t".to_string(),
        name.to_string(),
    ];
    wrap_for_target_noninteractive(target, args)
}

/// Kill a pane's tmux session only while it is still the generation that the
/// pane was authorized to control. A restored pane keeps its saved generation
/// separately from live probe metadata, so a refused same-name replacement
/// cannot be killed by child-exit, pane-close, tab-close, or workspace-close
/// cleanup.
pub fn kill_backing_command(backing: &crate::pane::TmuxBacking) -> Vec<String> {
    let Some(identity) = backing.authoritative_generation() else {
        return kill_session_command(&backing.target, &backing.session_name);
    };
    exact_kill_session_command(
        &backing.target,
        &backing.session_name,
        &identity.session_id,
        identity.session_created,
        &identity.continuity_id,
    )
    .unwrap_or_else(|| {
        wrap_for_target_noninteractive(
            &backing.target,
            vec![
                "tmux".to_string(),
                "display-message".to_string(),
                "-p".to_string(),
                "Close unavailable: the saved tmux generation is invalid.".to_string(),
            ],
        )
    })
}

fn exact_backing_mutation_command(
    backing: &crate::pane::TmuxBacking,
    authorized_command: String,
    mismatch_is_error: bool,
) -> Option<Vec<String>> {
    let identity = backing.authoritative_generation()?;
    let Some(condition) = exact_generation_condition(
        &identity.session_id,
        identity.session_created,
        &identity.continuity_id,
    ) else {
        return Some(wrap_for_target_noninteractive(
            &backing.target,
            vec![
                "tmux".to_string(),
                "run-shell".to_string(),
                "printf 'Exact tmux generation is invalid.\\n' >&2; exit 75".to_string(),
            ],
        ));
    };
    let unavailable = if mismatch_is_error {
        "run-shell \"printf 'Exact saved tmux target no longer exists.\\n' >&2; exit 75\""
    } else {
        "display-message -p 'Close skipped: the exact saved tmux target no longer exists.'"
    };
    Some(wrap_for_target_noninteractive(
        &backing.target,
        vec![
            "tmux".to_string(),
            "if-shell".to_string(),
            "-t".to_string(),
            backing.session_name.clone(),
            "-F".to_string(),
            condition,
            authorized_command,
            unavailable.to_string(),
        ],
    ))
}

pub fn resize_backing_command(
    backing: &crate::pane::TmuxBacking,
    cols: u32,
    rows: u32,
) -> Vec<String> {
    let escaped_name = agent_session_core::legacy::shell_escape(&backing.session_name);
    exact_backing_mutation_command(
        backing,
        format!("resize-pane -t {escaped_name} -x {cols} -y {rows}"),
        true,
    )
    .unwrap_or_else(|| resize_pane_command(&backing.target, &backing.session_name, cols, rows))
}

pub fn send_keys_backing_command(backing: &crate::pane::TmuxBacking, keys: &str) -> Vec<String> {
    let escaped_name = agent_session_core::legacy::shell_escape(&backing.session_name);
    let escaped_keys = agent_session_core::legacy::shell_escape(keys);
    exact_backing_mutation_command(
        backing,
        format!("send-keys -t {escaped_name} -l {escaped_keys}"),
        true,
    )
    .unwrap_or_else(|| send_keys_command(&backing.target, &backing.session_name, keys))
}

/// `tmux capture-pane -p -J -t {session} -l {lines}`
/// `-J` joins soft-wrapped lines so captures read as logical lines (EXAMPLE-89).
#[cfg(test)]
pub fn capture_pane_command(target: &TmuxTarget, session_name: &str, lines: u32) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "capture-pane".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-l".to_string(),
        lines.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux capture-pane -e -p -J -t {session}`
/// Preserves ANSI styling for the current visible pane contents. `-J` joins
/// soft-wrapped lines so the snapshot matches the soft-wrap-aware VTE capture
/// and lets the web client re-wrap responsively (EXAMPLE-89).
pub fn capture_pane_ansi_command(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "capture-pane".to_string(),
        "-e".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-t".to_string(),
        session_name.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux capture-pane -p -J -t {session}`
/// Captures the current visible screen as plain text for replace-style updates.
/// `-J` joins soft-wrapped lines so captures read as logical lines (EXAMPLE-89).
pub fn capture_pane_text_command(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "capture-pane".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-t".to_string(),
        session_name.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux resize-pane -t {session} -x {cols} -y {rows}`
pub fn resize_pane_command(
    target: &TmuxTarget,
    session_name: &str,
    cols: u32,
    rows: u32,
) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "resize-pane".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux send-keys -t {session} -l {keys}`
pub fn send_keys_command(target: &TmuxTarget, session_name: &str, keys: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "send-keys".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-l".to_string(),
        keys.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux has-session -t {session}` — non-interactive batch version for polling.
pub fn has_session_command_batch(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "has-session".to_string(),
        "-t".to_string(),
        session_name.to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux display-message -t {session} -p "#{pane_current_command}"` — non-interactive.
pub fn pane_current_command(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "display-message".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-p".to_string(),
        "#{pane_current_command}".to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux attach-session -t {session}` — interactive, requires PTY.
pub fn attach_command(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "attach-session".to_string(),
        "-t".to_string(),
        session_name.to_string(),
    ];
    wrap_for_target(target, tmux_args)
}

/// `tmux -V` — non-interactive availability/probe command.
pub fn version_command(target: &TmuxTarget) -> Vec<String> {
    let tmux_args = vec!["tmux".to_string(), "-V".to_string()];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// `tmux has-session -t {name}`
#[cfg(test)]
pub fn has_session_command(target: &TmuxTarget, name: &str) -> Vec<String> {
    let args = vec![
        "tmux".to_string(),
        "has-session".to_string(),
        "-t".to_string(),
        name.to_string(),
    ];
    wrap_for_target(target, args)
}

/// `tmux list-sessions -F "#{session_name}|#{session_created}|#{session_attached}|#{session_windows}"`
#[cfg(test)]
pub fn list_sessions_command(target: &TmuxTarget) -> Vec<String> {
    let args = vec![
        "tmux".to_string(),
        "list-sessions".to_string(),
        "-F".to_string(),
        "#{session_name}|#{session_created}|#{session_attached}|#{session_windows}".to_string(),
    ];
    wrap_for_target(target, args)
}

/// `tmux display-message -t {session} -p "#{pane_current_command}<sep>#{pane_current_path}<sep>#{pane_pid}<sep>#{pane_width}<sep>#{pane_height}<sep>#{session_id}<sep>#{session_created}<sep>#{@taarof-continuity-id}"`
pub fn pane_info_command(target: &TmuxTarget, session_name: &str) -> Vec<String> {
    let format = format!(
        "#{{pane_current_command}}{sep}#{{pane_current_path}}{sep}#{{pane_pid}}{sep}#{{pane_width}}{sep}#{{pane_height}}{sep}#{{session_id}}{sep}#{{session_created}}{sep}#{{@taarof-continuity-id}}",
        sep = PANE_INFO_SEPARATOR,
    );
    let args = vec![
        "tmux".to_string(),
        "display-message".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-p".to_string(),
        format,
    ];
    wrap_for_target_noninteractive(target, args)
}

/// Information about a tmux session.
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
pub struct TmuxSessionInfo {
    pub name: String,
    pub created_at: u64,
    pub attached_clients: u32,
    pub window_count: u32,
}

/// Information about the active pane in a tmux session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxPaneInfo {
    pub current_command: String,
    pub cwd: String,
    pub pid: i32,
    pub width: u32,
    pub height: u32,
    /// Server-scoped tmux identity. The pair is persisted with a layout so a
    /// later same-name session cannot be mistaken for the original process.
    pub session_id: String,
    pub session_created: u64,
    pub continuity_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxPaneSnapshot {
    pub output: String,
    pub width: u32,
    pub height: u32,
}

/// Parse the output of `list_sessions_command`.
/// Each line is: `name|created_at|attached_clients|window_count`.
/// Malformed lines are skipped.
#[cfg(test)]
pub fn parse_list_sessions(output: &str) -> Vec<TmuxSessionInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() != 4 {
                return None;
            }
            let name = parts[0].to_string();
            let created_at = parts[1].parse::<u64>().ok()?;
            let attached_clients = parts[2].parse::<u32>().ok()?;
            let window_count = parts[3].parse::<u32>().ok()?;
            Some(TmuxSessionInfo {
                name,
                created_at,
                attached_clients,
                window_count,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneInfoEncoding {
    PrintableV1,
    LegacyUnitSeparator,
    LegacyEscapedOctal,
}

impl PaneInfoEncoding {
    const SUPPORTED: [(Self, &'static str); 3] = [
        (Self::PrintableV1, PANE_INFO_SEPARATOR),
        (Self::LegacyUnitSeparator, LEGACY_PANE_INFO_SEPARATOR),
        (Self::LegacyEscapedOctal, "\\037"),
    ];
}

/// Parse the output of `pane_info_command`.
/// Expected format: `current_command<sep>cwd<sep>pid<sep>width<sep>height<sep>session_id<sep>session_created<sep>continuity_id`.
/// Returns None if the first line is malformed, missing, or mixes encodings.
pub fn parse_pane_info(output: &str) -> Option<TmuxPaneInfo> {
    parse_pane_info_with_encoding(output).map(|(info, _)| info)
}

fn parse_pane_info_with_encoding(output: &str) -> Option<(TmuxPaneInfo, PaneInfoEncoding)> {
    let line = output.lines().next()?;
    let mut present = PaneInfoEncoding::SUPPORTED
        .into_iter()
        .filter(|(_, separator)| line.contains(separator));
    let (encoding, separator) = present.next()?;
    if present.next().is_some() {
        return None;
    }
    parse_pane_info_with_separator(line, separator).map(|info| (info, encoding))
}

fn parse_pane_info_with_separator(line: &str, separator: &str) -> Option<TmuxPaneInfo> {
    if line.matches(separator).count() != 7 {
        return None;
    }
    let mut parts = line.split(separator);
    let current_command = parts.next()?.to_string();
    let cwd = parts.next()?.to_string();
    let pid = parts.next()?.parse::<i32>().ok()?;
    let width = parts.next()?.parse::<u32>().ok()?;
    let height = parts.next()?.parse::<u32>().ok()?;
    let session_id = parts.next()?.to_string();
    let session_created = parts.next()?.parse::<u64>().ok()?;
    let continuity_id = parts
        .next()
        .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string);
    if session_id.is_empty() || !session_id.starts_with('$') {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(TmuxPaneInfo {
        current_command,
        cwd,
        pid,
        width,
        height,
        session_id,
        session_created,
        continuity_id,
    })
}

/// Attach only when the tmux server still owns the exact saved generation.
/// The condition and attach command are submitted through one tmux client, so
/// a server replacement cannot race between a separate check and attach.
pub fn exact_attach_command(
    target: &TmuxTarget,
    session_name: &str,
    session_id: &str,
    session_created: u64,
    continuity_id: &str,
) -> Option<Vec<String>> {
    if session_name.is_empty() {
        return None;
    }
    let condition = exact_generation_condition(session_id, session_created, continuity_id)?;
    let escaped_name = agent_session_core::legacy::shell_escape(session_name);
    let tmux_args = vec![
        "tmux".to_string(),
        "if-shell".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        "-F".to_string(),
        condition,
        format!("attach-session -t {escaped_name}"),
        format!(
            "display-message -p '{}' ; run-shell 'exit 75'",
            EXACT_ATTACH_UNAVAILABLE_REASON
        ),
    ];
    Some(wrap_for_target(target, tmux_args))
}

fn exact_generation_condition(
    session_id: &str,
    session_created: u64,
    continuity_id: &str,
) -> Option<String> {
    if session_id.is_empty()
        || !session_id.starts_with('$')
        || !session_id[1..].bytes().all(|byte| byte.is_ascii_digit())
        || continuity_id.len() != 32
        || !continuity_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!(
        "#{{&&:#{{==:#{{@taarof-continuity-id}},{continuity_id}}},#{{&&:#{{==:#{{session_id}},{session_id}}},#{{==:#{{session_created}},{session_created}}}}}}}"
    ))
}

/// Kill only the exact generation through one tmux client. The false branch
/// is deliberately non-mutating: it leaves a same-name replacement intact.
pub fn exact_kill_session_command(
    target: &TmuxTarget,
    session_name: &str,
    session_id: &str,
    session_created: u64,
    continuity_id: &str,
) -> Option<Vec<String>> {
    if session_name.is_empty() {
        return None;
    }
    let condition = exact_generation_condition(session_id, session_created, continuity_id)?;
    let escaped_name = agent_session_core::legacy::shell_escape(session_name);
    Some(wrap_for_target_noninteractive(
        target,
        vec![
            "tmux".to_string(),
            "if-shell".to_string(),
            "-t".to_string(),
            session_name.to_string(),
            "-F".to_string(),
            condition,
            format!("kill-session -t {escaped_name}"),
            "display-message -p 'Close skipped: the exact saved tmux target no longer exists.'"
                .to_string(),
        ],
    ))
}

/// `tmux list-sessions -F "#{session_name}:#{session_created}:#{session_attached}:#{session_windows}"`
/// Uses colon separator and non-interactive SSH for dashboard polling.
pub fn list_sessions_dashboard_command(target: &TmuxTarget) -> Vec<String> {
    let tmux_args = vec![
        "tmux".to_string(),
        "list-sessions".to_string(),
        "-F".to_string(),
        "#{session_name}:#{session_created}:#{session_attached}:#{session_windows}".to_string(),
    ];
    wrap_for_target_noninteractive(target, tmux_args)
}

/// Parse a complete dashboard `list-sessions` response.
///
/// Each line is `name:created_at:attached:window_count`. The entire response is
/// rejected when any non-empty line is malformed, allowing callers to treat
/// only successful results as authoritative target snapshots.
pub fn parse_list_sessions_tuples(output: &str) -> Result<Vec<(String, u64, bool, u32)>, String> {
    output
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(line_index, line)| {
            let mut parts = line.splitn(4, ':');
            let name = parts.next().unwrap_or_default();
            let created = parts.next();
            let attached = parts.next();
            let windows = parts.next();
            if name.is_empty() || created.is_none() || attached.is_none() || windows.is_none() {
                return Err(format!(
                    "malformed tmux list-sessions output at line {}",
                    line_index + 1
                ));
            }
            let created = created
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    format!(
                        "malformed tmux list-sessions output at line {}",
                        line_index + 1
                    )
                })?;
            let attached = match attached {
                Some("0") => false,
                Some("1") => true,
                _ => {
                    return Err(format!(
                        "malformed tmux list-sessions output at line {}",
                        line_index + 1
                    ));
                }
            };
            let windows = windows
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| {
                    format!(
                        "malformed tmux list-sessions output at line {}",
                        line_index + 1
                    )
                })?;
            Ok((name.to_string(), created, attached, windows))
        })
        .collect()
}

/// Returns true if the command string is a known interactive shell.
pub fn is_shell_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh"
    )
}

pub fn capture_pane_snapshot(
    target: &TmuxTarget,
    session_name: &str,
    preserve_ansi: bool,
) -> Result<TmuxPaneSnapshot, String> {
    let pane_info = run_tmux_command_sync_result(&pane_info_command(target, session_name))
        .and_then(|output| {
            parse_pane_info(&output)
                .ok_or_else(|| "tmux pane probe returned invalid metadata".to_string())
        })?;

    let capture_command = if preserve_ansi {
        capture_pane_ansi_command(target, session_name)
    } else {
        capture_pane_text_command(target, session_name)
    };
    let output = run_tmux_command_sync_result(&capture_command)?;

    Ok(TmuxPaneSnapshot {
        output,
        width: pane_info.width,
        height: pane_info.height,
    })
}

fn run_tmux_command_sync_result(argv: &[String]) -> Result<String, String> {
    if argv.is_empty() {
        crate::diagnostics::record_command_failure(
            "http",
            "tmux-command",
            "refused to run an empty tmux command argv",
            None,
        );
        return Err("command argv was empty".to_string());
    }

    // Reaped by the try_wait() loop below, which also drains the pipes.
    #[allow(clippy::disallowed_methods)]
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            crate::diagnostics::record_command_failure(
                "http",
                "tmux-command",
                format!("failed to spawn tmux-related command {}", argv[0]),
                Some(serde_json::json!({
                    "argv": argv,
                    "error": error.to_string(),
                })),
            );
            format!("failed to spawn command: {error}")
        })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "failed to capture command stdout".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        "failed to capture command stderr".to_string()
    })?;
    let mut stdout_reader = Some(read_command_pipe(stdout));
    let mut stderr_reader = Some(read_command_pipe(stderr));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = collect_command_pipe(
                    stdout_reader.take().expect("stdout reader should exist"),
                    "stdout",
                    argv,
                )?;
                let stderr = collect_command_pipe(
                    stderr_reader.take().expect("stderr reader should exist"),
                    "stderr",
                    argv,
                )?;
                let stdout = String::from_utf8_lossy(&stdout).into_owned();
                let stderr = String::from_utf8_lossy(&stderr).into_owned();
                let trimmed_stderr = stderr.trim();

                if !status.success() {
                    let error = if trimmed_stderr.is_empty() {
                        format!("command exited with status {status}")
                    } else {
                        format!("command exited with status {status}: {trimmed_stderr}")
                    };
                    crate::diagnostics::record_command_failure(
                        "http",
                        "tmux-command",
                        format!("tmux-related command {} exited unsuccessfully", argv[0]),
                        Some(serde_json::json!({
                            "argv": argv,
                            "status": status.to_string(),
                            "exit_code": status.code(),
                            "stderr": if trimmed_stderr.is_empty() {
                                None::<String>
                            } else {
                                Some(trimmed_stderr.to_string())
                            },
                        })),
                    );
                    return Err(error);
                }

                return Ok(stdout);
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                cleanup_command_process(&mut child, stdout_reader.take(), stderr_reader.take());
                crate::diagnostics::record_command_failure(
                    "http",
                    "tmux-command",
                    format!("tmux-related command {} timed out", argv[0]),
                    Some(serde_json::json!({
                        "argv": argv,
                        "timeout_secs": 10,
                    })),
                );
                return Err("command timed out after 10s".to_string());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(error) => {
                cleanup_command_process(&mut child, stdout_reader.take(), stderr_reader.take());
                crate::diagnostics::record_command_failure(
                    "http",
                    "tmux-command",
                    format!("failed while waiting for tmux-related command {}", argv[0]),
                    Some(serde_json::json!({
                        "argv": argv,
                        "error": error.to_string(),
                    })),
                );
                return Err(format!("failed to wait for command: {error}"));
            }
        }
    }
}

fn cleanup_command_process(
    child: &mut std::process::Child,
    stdout_reader: Option<std::thread::JoinHandle<Result<Vec<u8>, String>>>,
    stderr_reader: Option<std::thread::JoinHandle<Result<Vec<u8>, String>>>,
) {
    let _ = child.kill();
    let _ = child.wait();
    if let Some(reader) = stdout_reader {
        let _ = reader.join();
    }
    if let Some(reader) = stderr_reader {
        let _ = reader.join();
    }
}

fn read_command_pipe<R>(mut pipe: R) -> std::thread::JoinHandle<Result<Vec<u8>, String>>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)
            .map_err(|error| format!("failed to read command output: {error}"))?;
        Ok(bytes)
    })
}

fn collect_command_pipe(
    reader: std::thread::JoinHandle<Result<Vec<u8>, String>>,
    stream_name: &str,
    argv: &[String],
) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| {
            crate::diagnostics::record_command_failure(
                "http",
                "tmux-command",
                format!("failed to join tmux-related command {stream_name} reader"),
                Some(serde_json::json!({
                    "argv": argv,
                })),
            );
            format!("failed to collect command {stream_name}")
        })?
        .map_err(|error| {
            crate::diagnostics::record_command_failure(
                "http",
                "tmux-command",
                format!("failed to read tmux-related command {stream_name}"),
                Some(serde_json::json!({
                    "argv": argv,
                    "error": error,
                })),
            );
            format!("failed to collect command {stream_name}")
        })
}

/// Stable identity for one coalescible tmux operation. The key is deliberately
/// made only from immutable command inputs; GTK objects and `AppState` never
/// cross into the worker.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TmuxJobKey {
    ValidateTarget(TmuxTarget),
    HasSession(TmuxTarget, String),
    KillSession(TmuxTarget, String),
    ClosePane {
        tab_id: u32,
        pane_id: u32,
    },
    CloseTab(u32),
    CloseWorkspace(u32),
    ResizePane(TmuxTarget, String, u32, u32),
    #[cfg(test)]
    Test(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmuxCommandOutcome {
    pub(crate) success: bool,
    pub(crate) status: Option<String>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) error: Option<String>,
}

impl TmuxCommandOutcome {
    fn deadline_exceeded() -> Self {
        Self {
            success: false,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("tmux worker deadline exceeded before command started".to_string()),
        }
    }

    pub(crate) fn result(&self) -> Result<&str, String> {
        if self.success {
            return Ok(&self.stdout);
        }
        if let Some(error) = self.error.as_ref() {
            return Err(error.clone());
        }
        let detail = self.stderr.trim();
        if !detail.is_empty() {
            return Err(detail.to_string());
        }
        let detail = self.stdout.trim();
        if !detail.is_empty() {
            return Err(detail.to_string());
        }
        Err(self.status.as_ref().map_or_else(
            || "tmux command failed".to_string(),
            |status| format!("command exited with {status}"),
        ))
    }
}

pub(crate) trait TmuxCommandAdapter: Send + Sync + 'static {
    fn execute(&self, argv: &[String], timeout: std::time::Duration) -> TmuxCommandOutcome;
}

struct ProcessTmuxAdapter;

impl TmuxCommandAdapter for ProcessTmuxAdapter {
    fn execute(&self, argv: &[String], timeout: std::time::Duration) -> TmuxCommandOutcome {
        run_process_tmux_command(argv, timeout.min(std::time::Duration::from_secs(10)))
    }
}

fn run_process_tmux_command(argv: &[String], timeout: std::time::Duration) -> TmuxCommandOutcome {
    if argv.is_empty() {
        return TmuxCommandOutcome {
            success: false,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("command argv was empty".to_string()),
        };
    }

    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::child_env::prepare_child_command(&mut command, &[]);
    // The bounded wait below owns and reaps the child on every path.
    #[allow(clippy::disallowed_methods)]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return TmuxCommandOutcome {
                success: false,
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to spawn command: {error}")),
            };
        }
    };
    let stdout_reader = child.stdout.take().map(read_command_pipe);
    let stderr_reader = child.stderr.take().map(read_command_pipe);
    let deadline = std::time::Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_reader
                    .map(|reader| collect_command_pipe(reader, "stdout", argv))
                    .transpose();
                let stderr = stderr_reader
                    .map(|reader| collect_command_pipe(reader, "stderr", argv))
                    .transpose();
                return match (stdout, stderr) {
                    (Ok(stdout), Ok(stderr)) => TmuxCommandOutcome {
                        success: status.success(),
                        status: Some(status.to_string()),
                        stdout: String::from_utf8_lossy(&stdout.unwrap_or_default()).into_owned(),
                        stderr: String::from_utf8_lossy(&stderr.unwrap_or_default()).into_owned(),
                        error: None,
                    },
                    (Err(error), _) | (_, Err(error)) => TmuxCommandOutcome {
                        success: false,
                        status: Some(status.to_string()),
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(error),
                    },
                };
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                cleanup_command_process(&mut child, stdout_reader, stderr_reader);
                return TmuxCommandOutcome {
                    success: false,
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!(
                        "command timed out after {:.3}s",
                        timeout.as_secs_f64()
                    )),
                };
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(error) => {
                cleanup_command_process(&mut child, stdout_reader, stderr_reader);
                return TmuxCommandOutcome {
                    success: false,
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("failed to wait for command: {error}")),
                };
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WorkerJobKey {
    Coalesced(TmuxJobKey, Vec<Vec<String>>),
    Unique(u64),
}

type WorkerReply =
    tokio::sync::oneshot::Sender<Result<std::sync::Arc<Vec<TmuxCommandOutcome>>, String>>;

struct WorkerWaiter {
    reply: WorkerReply,
}

struct QueuedTmuxJob {
    key: WorkerJobKey,
    commands: Vec<Vec<String>>,
    deadline_at: std::time::Instant,
    waiters: Vec<WorkerWaiter>,
}

#[derive(Default)]
struct TmuxWorkerState {
    active: usize,
    next_unique: u64,
    next_generation: u64,
    pending: std::collections::VecDeque<QueuedTmuxJob>,
    active_waiters: std::collections::HashMap<WorkerJobKey, Vec<WorkerWaiter>>,
    latest_generation: std::collections::HashMap<TmuxJobKey, u64>,
}

struct TmuxWorkerInner {
    adapter: std::sync::Arc<dyn TmuxCommandAdapter>,
    state: RefCell<TmuxWorkerState>,
    max_active: usize,
    max_pending: usize,
}

/// Bounded main-context scheduler for blocking tmux/SSH commands.
///
/// Only immutable argv requests reach `TmuxCommandAdapter`. Results return to
/// the GLib main context, where callers may use the generation on
/// [`TmuxCompletion`] plus their weak window identity before applying GTK or
/// `AppState` changes.
#[derive(Clone)]
pub(crate) struct TmuxWorker {
    inner: Rc<TmuxWorkerInner>,
}

pub(crate) struct TmuxCompletion {
    outcomes: std::sync::Arc<Vec<TmuxCommandOutcome>>,
    key: Option<TmuxJobKey>,
    generation: Option<u64>,
    worker: std::rc::Weak<TmuxWorkerInner>,
}

impl TmuxCompletion {
    pub(crate) fn outcomes(&self) -> &[TmuxCommandOutcome] {
        &self.outcomes
    }

    pub(crate) fn is_current(&self) -> bool {
        let (Some(key), Some(generation), Some(worker)) =
            (&self.key, self.generation, self.worker.upgrade())
        else {
            return true;
        };
        let current = worker
            .state
            .borrow()
            .latest_generation
            .get(key)
            .is_some_and(|latest| *latest == generation);
        current
    }
}

/// Main-context-only guard paired with a worker completion. It keeps GTK and
/// `AppState` out of the blocking request, then requires both the latest
/// operation generation and the originating live window before state/UI apply.
#[derive(Clone)]
pub(crate) struct TmuxGtkApplyGuard {
    state: std::rc::Weak<RefCell<crate::AppState>>,
    origin: TmuxApplyOrigin,
}

#[derive(Clone)]
enum TmuxApplyOrigin {
    Gtk(glib::WeakRef<adw::ApplicationWindow>),
    Generation {
        current: std::rc::Weak<std::cell::Cell<u64>>,
        expected: u64,
    },
}

impl TmuxGtkApplyGuard {
    pub(crate) fn new(
        state: &Rc<RefCell<crate::AppState>>,
        window: &adw::ApplicationWindow,
    ) -> Self {
        use glib::object::ObjectExt;
        Self {
            state: Rc::downgrade(state),
            origin: TmuxApplyOrigin::Gtk(window.downgrade()),
        }
    }

    /// Headless production surfaces use an explicit origin generation in
    /// place of a GTK weak window. This keeps the same completion-generation,
    /// state-liveness, and origin-liveness checks testable without a display.
    pub(crate) fn for_generation(
        state: &Rc<RefCell<crate::AppState>>,
        current: &Rc<std::cell::Cell<u64>>,
    ) -> Self {
        Self {
            state: Rc::downgrade(state),
            origin: TmuxApplyOrigin::Generation {
                current: Rc::downgrade(current),
                expected: current.get(),
            },
        }
    }

    pub(crate) fn upgrade_state(
        &self,
        completion: &TmuxCompletion,
    ) -> Option<Rc<RefCell<crate::AppState>>> {
        if !completion.is_current() {
            return None;
        }
        match &self.origin {
            TmuxApplyOrigin::Gtk(window) => {
                window.upgrade()?;
            }
            TmuxApplyOrigin::Generation { current, expected } => {
                let current = current.upgrade()?;
                if current.get() != *expected {
                    return None;
                }
            }
        }
        self.state.upgrade()
    }

    pub(crate) fn upgrade(
        &self,
        completion: &TmuxCompletion,
    ) -> Option<(Rc<RefCell<crate::AppState>>, adw::ApplicationWindow)> {
        let state = self.upgrade_state(completion)?;
        let TmuxApplyOrigin::Gtk(window) = &self.origin else {
            return None;
        };
        let window = window.upgrade()?;
        Some((state, window))
    }
}

impl TmuxWorker {
    pub(crate) fn with_adapter(
        adapter: std::sync::Arc<dyn TmuxCommandAdapter>,
        max_active: usize,
        max_pending: usize,
    ) -> Self {
        Self {
            inner: Rc::new(TmuxWorkerInner {
                adapter,
                state: RefCell::new(TmuxWorkerState::default()),
                max_active: max_active.max(1),
                max_pending: max_pending.max(1),
            }),
        }
    }

    pub(crate) async fn submit_coalesced(
        &self,
        key: TmuxJobKey,
        commands: Vec<Vec<String>>,
        deadline: std::time::Duration,
    ) -> Result<TmuxCompletion, String> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let generation = {
            let mut state = self.inner.state.borrow_mut();
            state.next_generation = state.next_generation.wrapping_add(1);
            let generation = state.next_generation;
            state.latest_generation.insert(key.clone(), generation);
            generation
        };
        self.enqueue(
            WorkerJobKey::Coalesced(key.clone(), commands.clone()),
            commands,
            deadline,
            WorkerWaiter { reply },
        );
        let outcomes = receiver
            .await
            .map_err(|_| "tmux worker reply channel closed".to_string())??;
        Ok(TmuxCompletion {
            outcomes,
            key: Some(key),
            generation: Some(generation),
            worker: Rc::downgrade(&self.inner),
        })
    }

    pub(crate) async fn submit(
        &self,
        commands: Vec<Vec<String>>,
        deadline: std::time::Duration,
    ) -> Result<TmuxCompletion, String> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let key = {
            let mut state = self.inner.state.borrow_mut();
            state.next_unique = state.next_unique.wrapping_add(1);
            WorkerJobKey::Unique(state.next_unique)
        };
        self.enqueue(key, commands, deadline, WorkerWaiter { reply });
        let outcomes = receiver
            .await
            .map_err(|_| "tmux worker reply channel closed".to_string())??;
        Ok(TmuxCompletion {
            outcomes,
            key: None,
            generation: None,
            worker: Rc::downgrade(&self.inner),
        })
    }

    fn enqueue(
        &self,
        key: WorkerJobKey,
        commands: Vec<Vec<String>>,
        deadline: std::time::Duration,
        waiter: WorkerWaiter,
    ) {
        let mut rejected = None;
        {
            let mut state = self.inner.state.borrow_mut();
            if let Some(waiters) = state.active_waiters.get_mut(&key) {
                waiters.push(waiter);
                return;
            }
            if let Some(pending) = state.pending.iter_mut().find(|job| job.key == key) {
                pending.waiters.push(waiter);
                return;
            }
            if state.pending.len() >= self.inner.max_pending {
                rejected = Some(waiter);
            } else {
                state.pending.push_back(QueuedTmuxJob {
                    key,
                    commands,
                    deadline_at: std::time::Instant::now() + deadline,
                    waiters: vec![waiter],
                });
            }
        }
        if let Some(waiter) = rejected {
            let _ = waiter
                .reply
                .send(Err("tmux worker queue is full".to_string()));
            return;
        }
        self.pump();
    }

    fn pump(&self) {
        loop {
            let job = {
                let mut state = self.inner.state.borrow_mut();
                if state.active >= self.inner.max_active {
                    return;
                }
                let Some(mut job) = state.pending.pop_front() else {
                    return;
                };
                state.active += 1;
                state
                    .active_waiters
                    .insert(job.key.clone(), std::mem::take(&mut job.waiters));
                job
            };

            let worker = self.clone();
            let adapter = self.inner.adapter.clone();
            glib::spawn_future_local(async move {
                let key = job.key;
                let result = gio::spawn_blocking(move || {
                    execute_worker_job(adapter.as_ref(), job.commands, job.deadline_at)
                })
                .await
                .map(std::sync::Arc::new)
                .map_err(|error| format!("tmux worker task failed: {error:?}"));
                worker.finish(key, result);
            });
        }
    }

    fn finish(
        &self,
        key: WorkerJobKey,
        result: Result<std::sync::Arc<Vec<TmuxCommandOutcome>>, String>,
    ) {
        let waiters = {
            let mut state = self.inner.state.borrow_mut();
            state.active = state.active.saturating_sub(1);
            state.active_waiters.remove(&key).unwrap_or_default()
        };
        for waiter in waiters {
            let _ = waiter.reply.send(result.clone());
        }
        self.pump();
    }
}

pub(crate) fn default_worker() -> TmuxWorker {
    thread_local! {
        static WORKER: std::cell::OnceCell<TmuxWorker> = const { std::cell::OnceCell::new() };
    }
    WORKER.with(|worker| {
        worker
            .get_or_init(|| {
                TmuxWorker::with_adapter(std::sync::Arc::new(ProcessTmuxAdapter), 1, 64)
            })
            .clone()
    })
}

fn execute_worker_job(
    adapter: &dyn TmuxCommandAdapter,
    commands: Vec<Vec<String>>,
    deadline_at: std::time::Instant,
) -> Vec<TmuxCommandOutcome> {
    commands
        .into_iter()
        .map(|argv| {
            let Some(remaining) = deadline_at.checked_duration_since(std::time::Instant::now())
            else {
                return TmuxCommandOutcome::deadline_exceeded();
            };
            if remaining.is_zero() {
                return TmuxCommandOutcome::deadline_exceeded();
            }
            adapter.execute(&argv, remaining)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_attach_for_test(
        target: &TmuxTarget,
        name: &str,
        cwd: Option<&str>,
        style: TmuxSessionStyle,
    ) -> Vec<String> {
        create_attach_command_with_continuity_id(target, name, cwd, style, None)
    }

    const TEST_CONTINUITY_ID: &str = "11111111111111111111111111111111";

    fn pane_info_line(command: &str, cwd: &str, separator: &str) -> String {
        format!(
            "{command}{separator}{cwd}{separator}12345{separator}120{separator}40{separator}$1{separator}1711720000{separator}{TEST_CONTINUITY_ID}"
        )
    }

    mod tmux_async {
        use super::*;

        struct DelayedFakeTmuxAdapter {
            delay: std::time::Duration,
            calls: std::sync::atomic::AtomicUsize,
            failure: Option<String>,
        }

        impl DelayedFakeTmuxAdapter {
            fn success_after(delay: std::time::Duration) -> Self {
                Self {
                    delay,
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    failure: None,
                }
            }

            fn failure_after(delay: std::time::Duration, error: &str) -> Self {
                Self {
                    delay,
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    failure: Some(error.to_string()),
                }
            }

            fn call_count(&self) -> usize {
                self.calls.load(std::sync::atomic::Ordering::Acquire)
            }
        }

        impl TmuxCommandAdapter for DelayedFakeTmuxAdapter {
            fn execute(
                &self,
                _argv: &[String],
                _timeout: std::time::Duration,
            ) -> TmuxCommandOutcome {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                std::thread::sleep(self.delay);
                if let Some(error) = self.failure.as_ref() {
                    return TmuxCommandOutcome {
                        success: false,
                        status: None,
                        stdout: String::new(),
                        stderr: error.clone(),
                        error: Some(error.clone()),
                    };
                }
                TmuxCommandOutcome {
                    success: true,
                    status: Some("exit status: 0".to_string()),
                    stdout: "tmux fake".to_string(),
                    stderr: String::new(),
                    error: None,
                }
            }
        }

        struct SelectiveFakeTmuxAdapter;

        impl TmuxCommandAdapter for SelectiveFakeTmuxAdapter {
            fn execute(
                &self,
                argv: &[String],
                _timeout: std::time::Duration,
            ) -> TmuxCommandOutcome {
                let failed = argv.iter().any(|arg| arg.contains("partial-fail"));
                TmuxCommandOutcome {
                    success: !failed,
                    status: Some(if failed {
                        "exit status: 1".to_string()
                    } else {
                        "exit status: 0".to_string()
                    }),
                    stdout: String::new(),
                    stderr: if failed {
                        "ssh timeout".to_string()
                    } else {
                        String::new()
                    },
                    error: failed.then(|| "ssh timeout".to_string()),
                }
            }
        }

        #[test]
        fn test_delayed_tmux_command_keeps_main_context_responsive() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(150),
            ));
            let worker = TmuxWorker::with_adapter(adapter, 1, 8);
            let heartbeat_count = std::rc::Rc::new(std::cell::Cell::new(0_u32));
            let heartbeat_count_for_tick = heartbeat_count.clone();
            let heartbeat =
                glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
                    heartbeat_count_for_tick.set(heartbeat_count_for_tick.get() + 1);
                    glib::ControlFlow::Continue
                });
            let completed = std::rc::Rc::new(std::cell::Cell::new(false));
            let completed_for_task = completed.clone();
            let worker_for_task = worker.clone();

            glib::spawn_future_local(async move {
                let completion = worker_for_task
                    .submit_coalesced(
                        TmuxJobKey::Test("delayed-heartbeat".to_string()),
                        vec![vec!["fake-tmux".to_string(), "-V".to_string()]],
                        std::time::Duration::from_secs(1),
                    )
                    .await
                    .expect("delayed fake command should complete");
                assert!(completion.outcomes()[0].success);
                assert!(completion.is_current());
                completed_for_task.set(true);
            });

            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !completed.get() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            heartbeat.remove();

            assert!(completed.get(), "delayed tmux work did not complete");
            assert!(
                heartbeat_count.get() >= 5,
                "GLib heartbeat stalled while tmux adapter slept: {} ticks",
                heartbeat_count.get()
            );
        }

        #[test]
        fn test_repeated_tmux_actions_coalesce_to_one_adapter_call() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(75),
            ));
            let worker = TmuxWorker::with_adapter(adapter.clone(), 1, 8);
            let completed = std::rc::Rc::new(std::cell::Cell::new(0_u32));

            for _ in 0..3 {
                let worker = worker.clone();
                let completed = completed.clone();
                glib::spawn_future_local(async move {
                    worker
                        .submit_coalesced(
                            TmuxJobKey::Test("same-kill".to_string()),
                            vec![vec!["fake-tmux".to_string(), "kill-session".to_string()]],
                            std::time::Duration::from_secs(1),
                        )
                        .await
                        .expect("coalesced request should complete");
                    completed.set(completed.get() + 1);
                });
            }

            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while completed.get() < 3 && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            assert_eq!(completed.get(), 3);
            assert_eq!(
                adapter.call_count(),
                1,
                "same in-flight action must coalesce"
            );
        }

        #[test]
        fn test_same_identity_with_changed_request_does_not_coalesce() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(40),
            ));
            let worker = TmuxWorker::with_adapter(adapter.clone(), 1, 8);
            let completed = Rc::new(std::cell::Cell::new(0_u32));
            for command in ["kill-old", "kill-new"] {
                let worker = worker.clone();
                let completed = completed.clone();
                glib::spawn_future_local(async move {
                    worker
                        .submit_coalesced(
                            TmuxJobKey::CloseTab(42),
                            vec![vec!["fake-tmux".to_string(), command.to_string()]],
                            std::time::Duration::from_secs(1),
                        )
                        .await
                        .expect("changed close request should complete");
                    completed.set(completed.get() + 1);
                });
            }
            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while completed.get() < 2 && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(completed.get(), 2);
            assert_eq!(adapter.call_count(), 2);
        }

        #[test]
        fn test_failed_async_kill_preserves_background() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::failure_after(
                std::time::Duration::from_millis(50),
                "ssh timeout",
            ));
            let worker = TmuxWorker::with_adapter(adapter, 1, 8);
            let mut app_state = crate::AppState::new();
            let workspace_id = app_state.active_workspace;
            let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "async-kill",
                crate::HeadlessPaneSeed {
                    tmux_session: Some("async-kill-session".to_string()),
                    tmux_ssh_target: Some("builder@example".to_string()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("tmux-backed tab should be seeded");
            let state = std::rc::Rc::new(std::cell::RefCell::new(app_state));
            let backing = state
                .borrow()
                .headless_pane(tab_id, pane_id)
                .and_then(|pane| pane.tmux_backing.clone())
                .expect("seeded pane should retain backing");
            let completed = std::rc::Rc::new(std::cell::Cell::new(false));
            let completed_for_task = completed.clone();
            let state_for_task = state.clone();
            let backing_for_task = backing.clone();

            glib::spawn_future_local(async move {
                let completion = worker
                    .submit_coalesced(
                        TmuxJobKey::ClosePane { tab_id, pane_id },
                        vec![kill_session_command(
                            &backing_for_task.target,
                            &backing_for_task.session_name,
                        )],
                        std::time::Duration::from_secs(1),
                    )
                    .await
                    .expect("fake kill should return a classified failure");
                let errors = crate::terminal::apply_tmux_kill_outcomes_before_removal(
                    &state_for_task,
                    std::slice::from_ref(&backing_for_task),
                    completion.outcomes(),
                    "default",
                );

                assert_eq!(errors.len(), 1);
                assert!(errors[0].contains("preserved it in Background"));
                assert!(
                    state_for_task.borrow().find_tab(tab_id).is_some(),
                    "failure must be registered before UI/state removal is finalized"
                );
                assert!(state_for_task
                    .borrow()
                    .detached_sessions
                    .iter()
                    .any(|session| session
                        .matches_target(&backing_for_task.session_name, &backing_for_task.target)));
                state_for_task.borrow_mut().remove_tab(tab_id);
                completed_for_task.set(true);
            });

            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !completed.get() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }

            assert!(completed.get());
            assert!(state.borrow().find_tab(tab_id).is_none());
            assert_eq!(state.borrow().detached_sessions.len(), 1);
        }

        #[test]
        fn test_partial_async_cleanup_preserves_only_failed_backing() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let worker =
                TmuxWorker::with_adapter(std::sync::Arc::new(SelectiveFakeTmuxAdapter), 1, 8);
            let state = Rc::new(RefCell::new(crate::AppState::new()));
            let backings = vec![
                crate::pane::TmuxBacking {
                    session_name: "partial-ok".to_string(),
                    target: TmuxTarget::Local,
                    expected_generation: None,
                    pane_info: crate::probe::ProbeSnapshot::default(),
                },
                crate::pane::TmuxBacking {
                    session_name: "partial-fail".to_string(),
                    target: TmuxTarget::Remote {
                        ssh_target: "builder@example".to_string(),
                    },
                    expected_generation: None,
                    pane_info: crate::probe::ProbeSnapshot::default(),
                },
            ];
            let completed = Rc::new(std::cell::Cell::new(false));
            let completed_for_task = completed.clone();
            let state_for_task = state.clone();
            let backings_for_task = backings.clone();
            glib::spawn_future_local(async move {
                let commands = backings_for_task
                    .iter()
                    .map(|backing| kill_session_command(&backing.target, &backing.session_name))
                    .collect();
                let completion = worker
                    .submit_coalesced(
                        TmuxJobKey::CloseTab(91),
                        commands,
                        std::time::Duration::from_secs(1),
                    )
                    .await
                    .expect("partial fake cleanup should complete");
                let errors = crate::terminal::apply_tmux_kill_outcomes_before_removal(
                    &state_for_task,
                    &backings_for_task,
                    completion.outcomes(),
                    "default",
                );
                assert_eq!(errors.len(), 1);
                completed_for_task.set(true);
            });
            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !completed.get() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(completed.get());
            let detached = &state.borrow().detached_sessions;
            assert_eq!(detached.len(), 1);
            assert!(detached[0].matches_target("partial-fail", &backings[1].target));
        }

        #[test]
        fn test_multi_backing_cleanup_stops_at_overall_deadline() {
            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(60),
            ));
            let worker = TmuxWorker::with_adapter(adapter.clone(), 1, 8);
            let outcomes = Rc::new(RefCell::new(None));
            let outcomes_for_task = outcomes.clone();
            glib::spawn_future_local(async move {
                let completion = worker
                    .submit_coalesced(
                        TmuxJobKey::CloseWorkspace(7),
                        vec![
                            vec!["fake-tmux".to_string(), "kill-one".to_string()],
                            vec!["fake-tmux".to_string(), "kill-two".to_string()],
                        ],
                        std::time::Duration::from_millis(30),
                    )
                    .await
                    .expect("bounded batch should return classified outcomes");
                *outcomes_for_task.borrow_mut() = Some(completion.outcomes().to_vec());
            });
            let context = glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while outcomes.borrow().is_none() && std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let outcomes = outcomes
                .borrow()
                .clone()
                .expect("bounded batch should complete");
            assert_eq!(outcomes.len(), 2);
            assert!(outcomes[0].success);
            assert!(outcomes[1]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("deadline exceeded")));
            assert_eq!(adapter.call_count(), 1, "later kill must not start late");
        }

        fn socket_request(
            socket_path: std::path::PathBuf,
            request: serde_json::Value,
        ) -> std::sync::mpsc::Receiver<serde_json::Value> {
            use std::io::{Read, Write};
            use std::net::Shutdown;
            use std::os::unix::net::UnixStream;

            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut stream =
                    UnixStream::connect(socket_path).expect("real socket client should connect");
                stream
                    .write_all(
                        serde_json::to_string(&request)
                            .expect("request should serialize")
                            .as_bytes(),
                    )
                    .expect("request should be written");
                stream
                    .shutdown(Shutdown::Write)
                    .expect("socket write side should close");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("response should be read");
                tx.send(serde_json::from_str(&response).expect("response should be JSON"))
                    .expect("response receiver should remain live");
            });
            rx
        }

        #[test]
        fn test_production_dispatch_real_socket_stays_responsive_during_delayed_tmux_close() {
            use std::os::unix::fs::PermissionsExt;

            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(250),
            ));
            let worker = TmuxWorker::with_adapter(adapter.clone(), 1, 8);
            let mut app_state = crate::AppState::new();
            let workspace_id = app_state.active_workspace;
            let (closing_tab, _) = crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "socket-close",
                crate::HeadlessPaneSeed {
                    tmux_session: Some("socket-close-session".to_string()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("tmux-backed close tab should be seeded");
            crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "survivor",
                crate::HeadlessPaneSeed::default(),
            )
            .expect("a surviving tab should be seeded");
            let state = Rc::new(RefCell::new(app_state));
            let runtime_dir = std::env::temp_dir().join(format!(
                "taarof-tmux-async-socket-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock should be valid")
                    .as_nanos()
            ));
            std::fs::create_dir(&runtime_dir).expect("runtime dir should be created");
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
                .expect("runtime dir permissions should be private");
            let (socket_path, _generation) = crate::socket::start_tmux_async_test_socket_server(
                state.clone(),
                &runtime_dir,
                worker,
                None,
            )
            .expect("real async test socket should start");
            let close_rx = socket_request(
                socket_path.clone(),
                serde_json::json!({"action": "close-tab", "tab": closing_tab.to_string()}),
            );
            let heartbeat_count = Rc::new(std::cell::Cell::new(0_u32));
            let heartbeat_for_tick = heartbeat_count.clone();
            let heartbeat =
                glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
                    heartbeat_for_tick.set(heartbeat_for_tick.get() + 1);
                    glib::ControlFlow::Continue
                });
            let context = glib::MainContext::default();
            let call_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while adapter.call_count() == 0 && std::time::Instant::now() < call_deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(adapter.call_count(), 1, "delayed close should have started");

            let query_rx = socket_request(
                socket_path.clone(),
                serde_json::json!({"action": "query-state"}),
            );
            let query_deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
            let query_response = loop {
                while context.pending() {
                    context.iteration(false);
                }
                if let Ok(response) = query_rx.try_recv() {
                    break response;
                }
                assert!(
                    std::time::Instant::now() < query_deadline,
                    "query-state stalled behind delayed tmux close"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert_eq!(query_response["ok"], true);
            assert!(
                close_rx.try_recv().is_err(),
                "delayed close unexpectedly finished before responsive query"
            );

            let close_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let close_response = loop {
                while context.pending() {
                    context.iteration(false);
                }
                if let Ok(response) = close_rx.try_recv() {
                    break response;
                }
                assert!(
                    std::time::Instant::now() < close_deadline,
                    "delayed close did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            heartbeat.remove();
            assert_eq!(close_response["ok"], true);
            assert!(heartbeat_count.get() >= 5);
            assert!(state.borrow().find_tab(closing_tab).is_none());
            crate::socket::cleanup_socket(&socket_path);
            std::fs::remove_dir(&runtime_dir).expect("runtime dir should be removable");
        }

        #[test]
        fn test_production_dispatch_generation_guard_rejects_late_tmux_close() {
            use std::os::unix::fs::PermissionsExt;

            let _glib_guard = crate::glib_main_context_test_guard();
            let adapter = std::sync::Arc::new(DelayedFakeTmuxAdapter::success_after(
                std::time::Duration::from_millis(150),
            ));
            let worker = TmuxWorker::with_adapter(adapter.clone(), 1, 8);
            let mut app_state = crate::AppState::new();
            let workspace_id = app_state.active_workspace;
            let (closing_tab, _) = crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "socket-generation-close",
                crate::HeadlessPaneSeed {
                    tmux_session: Some("socket-generation-session".to_string()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("tmux-backed close tab should be seeded");
            crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "survivor",
                crate::HeadlessPaneSeed::default(),
            )
            .expect("a surviving tab should be seeded");
            let state = Rc::new(RefCell::new(app_state));
            let runtime_dir = std::env::temp_dir().join(format!(
                "taarof-tmux-generation-socket-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock should be valid")
                    .as_nanos()
            ));
            std::fs::create_dir(&runtime_dir).expect("runtime dir should be created");
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
                .expect("runtime dir permissions should be private");
            let (socket_path, generation) = crate::socket::start_tmux_async_test_socket_server(
                state.clone(),
                &runtime_dir,
                worker,
                None,
            )
            .expect("production dispatch test socket should start");
            let close_rx = socket_request(
                socket_path.clone(),
                serde_json::json!({"action": "close-tab", "tab": closing_tab.to_string()}),
            );
            let context = glib::MainContext::default();
            let start_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while adapter.call_count() == 0 && std::time::Instant::now() < start_deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(adapter.call_count(), 1, "delayed close should have started");
            generation.set(generation.get().wrapping_add(1));

            let response_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let response = loop {
                while context.pending() {
                    context.iteration(false);
                }
                if let Ok(response) = close_rx.try_recv() {
                    break response;
                }
                assert!(
                    std::time::Instant::now() < response_deadline,
                    "superseded close did not reply"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert_eq!(response["ok"], false);
            assert!(response["error"]
                .as_str()
                .is_some_and(|error| error.contains("superseded")));
            assert!(
                state.borrow().find_tab(closing_tab).is_some(),
                "stale originating generation must not remove the tab"
            );
            crate::socket::cleanup_socket(&socket_path);
            std::fs::remove_dir(&runtime_dir).expect("runtime dir should be removable");
        }

        #[test]
        fn test_production_dispatch_partial_kill_preserves_background_before_removal() {
            use std::os::unix::fs::PermissionsExt;

            let _glib_guard = crate::glib_main_context_test_guard();
            let worker =
                TmuxWorker::with_adapter(std::sync::Arc::new(SelectiveFakeTmuxAdapter), 1, 8);
            let mut app_state = crate::AppState::new();
            let workspace_id = app_state.active_workspace;
            let (closing_tab, _) = crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "socket-partial-close",
                crate::HeadlessPaneSeed {
                    tmux_session: Some("partial-ok".to_string()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("tmux-backed close tab should be seeded");
            let failed_pane_id = app_state.next_id;
            app_state.next_id += 1;
            app_state.headless_panes.insert(
                (closing_tab, failed_pane_id),
                crate::runtime::HeadlessPaneState {
                    tmux_backing: Some(crate::pane::TmuxBacking {
                        session_name: "partial-fail".to_string(),
                        target: TmuxTarget::Remote {
                            ssh_target: "builder@example".to_string(),
                        },
                        expected_generation: None,
                        pane_info: crate::probe::ProbeSnapshot::default(),
                    }),
                    ..crate::runtime::HeadlessPaneState::default()
                },
            );
            crate::seed_headless_terminal_tab(
                &mut app_state,
                workspace_id,
                "survivor",
                crate::HeadlessPaneSeed::default(),
            )
            .expect("a surviving tab should be seeded");
            let state = Rc::new(RefCell::new(app_state));
            let ordering_observed = Rc::new(std::cell::Cell::new(false));
            let ordering_for_observer = ordering_observed.clone();
            let before_removal: crate::socket::BeforeTabRemovalObserver =
                Rc::new(move |state, tab_id| {
                    let failed_is_background = state.detached_sessions.iter().any(|session| {
                        session.session_name == "partial-fail"
                            && session.target
                                == (TmuxTarget::Remote {
                                    ssh_target: "builder@example".to_string(),
                                })
                    });
                    let successful_is_absent = state
                        .detached_sessions
                        .iter()
                        .all(|session| session.session_name != "partial-ok");
                    ordering_for_observer.set(
                        state.find_tab(tab_id).is_some()
                            && failed_is_background
                            && successful_is_absent,
                    );
                });
            let runtime_dir = std::env::temp_dir().join(format!(
                "taarof-tmux-partial-socket-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock should be valid")
                    .as_nanos()
            ));
            std::fs::create_dir(&runtime_dir).expect("runtime dir should be created");
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
                .expect("runtime dir permissions should be private");
            let (socket_path, _generation) = crate::socket::start_tmux_async_test_socket_server(
                state.clone(),
                &runtime_dir,
                worker,
                Some(before_removal),
            )
            .expect("production dispatch test socket should start");
            let close_rx = socket_request(
                socket_path.clone(),
                serde_json::json!({"action": "close-tab", "tab": closing_tab.to_string()}),
            );
            let context = glib::MainContext::default();
            let response_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let response = loop {
                while context.pending() {
                    context.iteration(false);
                }
                if let Ok(response) = close_rx.try_recv() {
                    break response;
                }
                assert!(
                    std::time::Instant::now() < response_deadline,
                    "partial close did not reply"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            assert_eq!(response["ok"], false);
            assert!(response["error"]
                .as_str()
                .is_some_and(|error| error.contains("preserved it in Background")));
            assert!(
                ordering_observed.get(),
                "failed backing must enter Background before the source tab is removed"
            );
            let state = state.borrow();
            assert!(state.find_tab(closing_tab).is_none());
            assert_eq!(state.detached_sessions.len(), 1);
            assert_eq!(state.detached_sessions[0].session_name, "partial-fail");
            drop(state);
            crate::socket::cleanup_socket(&socket_path);
            std::fs::remove_dir(&runtime_dir).expect("runtime dir should be removable");
        }
    }

    fn shell_join_for_ssh(argv: &[String]) -> String {
        argv.join(" ")
    }

    // --- session_name ---

    #[test]
    fn test_session_name_basic() {
        let name = session_name("taarof", "taarof-app", 1, 0);
        assert_eq!(name, "taarof--taarof-app--t1--0");
    }

    #[test]
    fn test_session_name_custom_prefix() {
        let name = session_name("myapp", "workspace", 1, 0);
        assert_eq!(name, "myapp--workspace--t1--0");
    }

    #[test]
    fn test_session_name_sanitizes_dots_and_colons() {
        let name = session_name("taarof", "my.project", 3, 2);
        assert_eq!(name, "taarof--my_project--t3--2");
    }

    #[test]
    fn test_session_name_unique_per_tab_id() {
        let a = session_name("taarof", "ws", 1, 0);
        let b = session_name("taarof", "ws", 2, 0);
        assert_ne!(
            a, b,
            "different tab_ids must produce different session names"
        );
    }

    #[test]
    fn test_session_name_truncates_long_names() {
        let workspace = "a".repeat(200);
        let name = session_name("taarof", &workspace, 1, 0);
        assert!(name.len() <= 128);
    }

    #[test]
    fn test_session_name_truncates_long_unicode_without_panic() {
        // Each '日' is 3 bytes UTF-8; 50 chars = 150 bytes, well over 128-byte limit.
        // Old code would panic slicing at a byte boundary mid-character.
        let workspace = "日".repeat(50);
        let name = session_name("taarof", &workspace, 1, 0);
        assert!(name.len() <= 128);
        assert!(name.is_char_boundary(name.len()));
    }

    // --- create_attach_command ---

    #[test]
    fn test_create_attach_command_local() {
        let cmd = create_attach_for_test(
            &TmuxTarget::Local,
            "my-session",
            Some("/tmp/user"),
            TmuxSessionStyle::Inherit,
        );
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user"
            ]
        );
    }

    #[test]
    fn test_create_attach_command_local_no_cwd() {
        let cmd = create_attach_for_test(
            &TmuxTarget::Local,
            "my-session",
            None,
            TmuxSessionStyle::Inherit,
        );
        assert_eq!(cmd, vec!["tmux", "new-session", "-As", "my-session"]);
    }

    #[test]
    fn test_create_attach_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = create_attach_for_test(
            &target,
            "my-session",
            Some("/tmp/user"),
            TmuxSessionStyle::Inherit,
        );
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-t",
                "user@host",
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user"
            ]
        );
    }

    #[test]
    fn create_attach_records_an_owned_generation() {
        let command = create_attach_command(
            &TmuxTarget::Local,
            "my-session",
            None,
            TmuxSessionStyle::Inherit,
        );
        let command_argument = command
            .iter()
            .find(|argument| argument.starts_with("set-option -t "))
            .expect("created sessions must carry a continuity generation");
        let generation = command_argument
            .split_whitespace()
            .last()
            .expect("continuity generation must be an argument");
        assert_eq!(generation.len(), 32);
        assert!(generation.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(command
            .iter()
            .any(|argument| argument == "#{==:#{@taarof-continuity-id},}"));
    }

    #[test]
    fn exact_attach_binds_nonce_even_when_tmux_ids_and_seconds_collide() {
        let original = exact_attach_command(
            &TmuxTarget::Local,
            "same-name",
            "$1",
            1711720000,
            TEST_CONTINUITY_ID,
        )
        .unwrap();
        let replacement = exact_attach_command(
            &TmuxTarget::Local,
            "same-name",
            "$1",
            1711720000,
            "22222222222222222222222222222222",
        )
        .unwrap();

        assert_ne!(original[5], replacement[5]);
        assert!(original[5].contains(TEST_CONTINUITY_ID));
        assert_eq!(original[1], "if-shell");
        assert_eq!(original[6], "attach-session -t same-name");
        assert!(original[7].contains("Reattach unavailable:"));
        assert!(original[7].contains("exit 75"));
        assert!(!original[7].contains("sleep"));
        assert!(exact_attach_command(
            &TmuxTarget::Local,
            "same-name",
            "$1",
            1711720000,
            "legacy-missing-generation",
        )
        .is_none());
    }

    #[test]
    fn restored_backing_cleanup_is_bound_to_the_saved_generation() {
        let mut backing = crate::pane::TmuxBacking {
            session_name: "same-name".into(),
            target: TmuxTarget::Local,
            expected_generation: Some(crate::session::SavedTmuxIdentity {
                session_id: "$1".into(),
                session_created: 1711720000,
                continuity_id: TEST_CONTINUITY_ID.into(),
            }),
            pane_info: crate::probe::ProbeSnapshot::default(),
        };

        backing.pane_info.record_success(TmuxPaneInfo {
            current_command: "zsh".into(),
            cwd: "/replacement".into(),
            pid: 42,
            width: 80,
            height: 24,
            session_id: "$1".into(),
            session_created: 1711720000,
            continuity_id: Some("22222222222222222222222222222222".into()),
        });
        assert_eq!(
            backing
                .authoritative_generation()
                .expect("saved authority must remain primary")
                .continuity_id,
            TEST_CONTINUITY_ID
        );

        let command = kill_backing_command(&backing);
        assert_eq!(command[0], "tmux");
        assert_eq!(command[1], "if-shell");
        assert_eq!(command[2..5], ["-t", "same-name", "-F"]);
        assert!(command[5].contains(TEST_CONTINUITY_ID));
        assert!(command[5].contains("#{session_id},$1"));
        assert!(command[5].contains("#{session_created},1711720000"));
        assert_eq!(command[6], "kill-session -t same-name");
        assert!(command[7].contains("Close skipped"));
        assert_ne!(command[1], "kill-session");

        let resize = resize_backing_command(&backing, 120, 40);
        assert_eq!(resize[1], "if-shell");
        assert!(resize[5].contains(TEST_CONTINUITY_ID));
        assert_eq!(resize[6], "resize-pane -t same-name -x 120 -y 40");
        assert!(resize[7].contains("exit 75"));

        let send = send_keys_backing_command(&backing, "text with 'quote'");
        assert_eq!(send[1], "if-shell");
        assert!(send[5].contains(TEST_CONTINUITY_ID));
        assert!(send[6].starts_with("send-keys -t same-name -l "));
        assert!(send[6].contains("'\"'\"'"));
        assert!(send[7].contains("exit 75"));
    }

    // --- session_style ---

    #[test]
    fn test_session_style_plain_builds_session_scoped_set_option_argv() {
        let cmd = create_attach_for_test(
            &TmuxTarget::Local,
            "my-session",
            Some("/tmp/user"),
            TmuxSessionStyle::Plain,
        );
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user",
                ";",
                "set-option",
                "-t",
                "my-session",
                "status",
                "off",
                ";",
                "set-option",
                "-t",
                "my-session",
                "mouse",
                "on",
            ]
        );
        assert!(
            !cmd.iter().any(|arg| arg == "-g"),
            "plain styling must stay session-scoped, never global: {cmd:?}"
        );

        // Without a cwd the styled suffix still follows the bare create/attach.
        let no_cwd = create_attach_for_test(
            &TmuxTarget::Local,
            "my-session",
            None,
            TmuxSessionStyle::Plain,
        );
        assert_eq!(
            no_cwd,
            vec![
                "tmux",
                "new-session",
                "-As",
                "my-session",
                ";",
                "set-option",
                "-t",
                "my-session",
                "status",
                "off",
                ";",
                "set-option",
                "-t",
                "my-session",
                "mouse",
                "on",
            ]
        );
    }

    #[test]
    fn test_session_style_inherit_leaves_create_attach_argv_unchanged() {
        // The exact argv taarof has always generated, spelled out so a future
        // change to the styled builder cannot silently alter the default.
        assert_eq!(
            create_attach_for_test(
                &TmuxTarget::Local,
                "my-session",
                Some("/tmp/user"),
                TmuxSessionStyle::Inherit,
            ),
            vec![
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user"
            ]
        );
        assert_eq!(
            create_attach_for_test(
                &TmuxTarget::Local,
                "my-session",
                None,
                TmuxSessionStyle::Inherit,
            ),
            vec!["tmux", "new-session", "-As", "my-session"]
        );
        let remote = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        assert_eq!(
            create_attach_for_test(
                &remote,
                "my-session",
                Some("/tmp/user"),
                TmuxSessionStyle::Inherit,
            ),
            vec![
                "ssh",
                "-t",
                "user@host",
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user"
            ]
        );

        // The default style is inherit, so an unconfigured install never grows
        // an extra argument on any target/cwd combination.
        assert_eq!(TmuxSessionStyle::default(), TmuxSessionStyle::Inherit);
        for target in [TmuxTarget::Local, remote] {
            for cwd in [Some("/tmp/user"), None] {
                assert_eq!(
                    create_attach_for_test(&target, "my-session", cwd, TmuxSessionStyle::default()),
                    create_attach_for_test(&target, "my-session", cwd, TmuxSessionStyle::Inherit),
                );
            }
        }

        // Attaching to a session taarof did not create is never styled.
        assert_eq!(
            attach_command(&TmuxTarget::Local, "someone-elses-session"),
            vec!["tmux", "attach-session", "-t", "someone-elses-session"]
        );
    }

    #[test]
    fn test_session_style_plain_wraps_remote_targets_with_ssh() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = create_attach_for_test(
            &target,
            "my-session",
            Some("/tmp/user"),
            TmuxSessionStyle::Plain,
        );
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-t",
                "user@host",
                "tmux",
                "new-session",
                "-As",
                "my-session",
                "-c",
                "/tmp/user",
                "';'",
                "set-option",
                "-t",
                "my-session",
                "status",
                "off",
                "';'",
                "set-option",
                "-t",
                "my-session",
                "mouse",
                "on",
            ]
        );
        // The chaining separator must reach the remote tmux as a literal
        // argument, so it has to stay quoted for the remote shell.
        assert!(
            !cmd.iter().any(|arg| arg == ";"),
            "an unquoted ';' would be a remote shell operator: {cmd:?}"
        );
        assert!(
            !cmd.iter().any(|arg| arg == "-g"),
            "plain styling must stay session-scoped, never global: {cmd:?}"
        );
    }

    // --- kill_session_command ---

    #[test]
    fn test_kill_command_local() {
        let cmd = kill_session_command(&TmuxTarget::Local, "my-session");
        assert_eq!(cmd, vec!["tmux", "kill-session", "-t", "my-session"]);
    }

    #[test]
    fn test_kill_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = kill_session_command(&target, "my-session");
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "user@host",
                "tmux",
                "kill-session",
                "-t",
                "my-session",
            ]
        );
    }

    // --- list_sessions_command ---

    #[test]
    fn test_list_sessions_command_local() {
        let cmd = list_sessions_command(&TmuxTarget::Local);
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "list-sessions",
                "-F",
                "#{session_name}|#{session_created}|#{session_attached}|#{session_windows}"
            ]
        );
    }

    // --- pane_info_command ---

    #[test]
    fn test_pane_info_command_local() {
        let cmd = pane_info_command(&TmuxTarget::Local, "my-session");
        let format = format!(
            "#{{pane_current_command}}{sep}#{{pane_current_path}}{sep}#{{pane_pid}}{sep}#{{pane_width}}{sep}#{{pane_height}}{sep}#{{session_id}}{sep}#{{session_created}}{sep}#{{@taarof-continuity-id}}",
            sep = PANE_INFO_SEPARATOR,
        );
        assert_eq!(
            cmd,
            vec!["tmux", "display-message", "-t", "my-session", "-p", &format,]
        );
    }

    #[test]
    fn test_pane_info_command_uses_printable_remote_safe_separator() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = pane_info_command(&target, "my-session");

        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "user@host",
                "tmux",
                "display-message",
                "-t",
                "my-session",
                "-p",
                "'#{pane_current_command}__TAAROF_PANE_INFO_V1__#{pane_current_path}__TAAROF_PANE_INFO_V1__#{pane_pid}__TAAROF_PANE_INFO_V1__#{pane_width}__TAAROF_PANE_INFO_V1__#{pane_height}__TAAROF_PANE_INFO_V1__#{session_id}__TAAROF_PANE_INFO_V1__#{session_created}__TAAROF_PANE_INFO_V1__#{@taarof-continuity-id}'",
            ]
        );
    }

    // --- has_session_command ---

    #[test]
    fn test_has_session_command_local() {
        let cmd = has_session_command(&TmuxTarget::Local, "my-session");
        assert_eq!(cmd, vec!["tmux", "has-session", "-t", "my-session"]);
    }

    #[test]
    fn test_has_session_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = has_session_command(&target, "my-session");
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-t",
                "user@host",
                "tmux",
                "has-session",
                "-t",
                "my-session"
            ]
        );
    }

    // --- parse_list_sessions ---

    #[test]
    fn test_parse_list_sessions_single() {
        let output = "mysession|1711720000|1|2";
        let sessions = parse_list_sessions(output);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0],
            TmuxSessionInfo {
                name: "mysession".to_string(),
                created_at: 1711720000,
                attached_clients: 1,
                window_count: 2,
            }
        );
    }

    #[test]
    fn test_parse_list_sessions_multiple() {
        let output = "session1|1711720000|0|1\nsession2|1711720100|2|3";
        let sessions = parse_list_sessions(output);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "session1");
        assert_eq!(sessions[1].name, "session2");
        assert_eq!(sessions[1].attached_clients, 2);
        assert_eq!(sessions[1].window_count, 3);
    }

    #[test]
    fn test_parse_list_sessions_empty() {
        let sessions = parse_list_sessions("");
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_parse_list_sessions_malformed_line_skipped() {
        let output = "good|1711720000|1|2\nbad_line_no_pipes\nalso|bad|not|numbers";
        let sessions = parse_list_sessions(output);
        // "good" parses fine; "bad_line_no_pipes" skipped; "also|bad|not|numbers" skipped
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "good");
    }

    // --- parse_pane_info ---

    #[test]
    fn test_parse_pane_info() {
        let info = parse_pane_info(&pane_info_line(
            "vim",
            "/tmp/user/project",
            PANE_INFO_SEPARATOR,
        ))
        .unwrap();
        assert_eq!(
            info,
            TmuxPaneInfo {
                current_command: "vim".to_string(),
                cwd: "/tmp/user/project".to_string(),
                pid: 12345,
                width: 120,
                height: 40,
                session_id: "$1".to_string(),
                session_created: 1711720000,
                continuity_id: Some(TEST_CONTINUITY_ID.to_string()),
            }
        );
    }

    #[test]
    fn test_parse_pane_info_shell() {
        let info =
            parse_pane_info(&pane_info_line("zsh", "/tmp/user", PANE_INFO_SEPARATOR)).unwrap();
        assert_eq!(info.current_command, "zsh");
        assert!(is_shell_command(&info.current_command));
        assert_eq!(info.cwd, "/tmp/user");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.width, 120);
        assert_eq!(info.height, 40);
    }

    #[test]
    fn test_parse_pane_info_accepts_legacy_unit_separator() {
        let (info, encoding) = parse_pane_info_with_encoding(&pane_info_line(
            "zsh",
            "/tmp/user/project|pipe",
            LEGACY_PANE_INFO_SEPARATOR,
        ))
        .expect("legacy pane metadata should parse");

        assert_eq!(encoding, PaneInfoEncoding::LegacyUnitSeparator);
        assert_eq!(info.current_command, "zsh");
        assert_eq!(info.cwd, "/tmp/user/project|pipe");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.width, 120);
        assert_eq!(info.height, 40);
    }

    #[test]
    fn test_parse_pane_info_malformed() {
        assert!(parse_pane_info("").is_none());
        assert!(parse_pane_info("only|two").is_none());
        assert!(parse_pane_info("a|b|123|80|24").is_none());
        assert!(parse_pane_info("a|b|notanumber|80|24").is_none());
    }

    #[test]
    fn test_parse_pane_info_rejects_mixed_and_mangled_separators() {
        let mixed = format!(
            "zsh{v1}/tmp/project{legacy}123{v1}80{v1}24",
            v1 = PANE_INFO_SEPARATOR,
            legacy = LEGACY_PANE_INFO_SEPARATOR,
        );
        assert!(parse_pane_info(&mixed).is_none());

        let too_many = format!(
            "zsh{sep}/tmp/contains{sep}token{sep}123{sep}80{sep}24",
            sep = PANE_INFO_SEPARATOR,
        );
        assert!(parse_pane_info(&too_many).is_none());

        let partial = format!(
            "zsh{sep}/tmp/project{sep}123{sep}80|24",
            sep = PANE_INFO_SEPARATOR,
        );
        assert!(parse_pane_info(&partial).is_none());
    }

    #[test]
    fn test_pane_info_command_uses_non_path_separator() {
        let cmd = pane_info_command(&TmuxTarget::Local, "session-a");
        let format = cmd.last().expect("format argument should exist");
        assert!(format.contains(PANE_INFO_SEPARATOR));
        assert!(!format.contains("#{pane_current_command}|#{pane_current_path}"));
    }

    #[test]
    fn test_parse_pane_info_allows_pipe_in_path() {
        let info = parse_pane_info(&pane_info_line(
            "vim",
            "/tmp/project|with-pipe",
            PANE_INFO_SEPARATOR,
        ))
        .unwrap();
        assert_eq!(info.cwd, "/tmp/project|with-pipe");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.width, 120);
        assert_eq!(info.height, 40);
    }

    #[test]
    fn test_parse_pane_info_accepts_remote_shell_octal_separator() {
        let (info, encoding) = parse_pane_info_with_encoding(
            "zsh\\037/tmp/user/project|pipe\\03712345\\037120\\03740\\037$1\\0371711720000\\03711111111111111111111111111111111",
        )
        .expect("remote pane metadata should parse");
        assert_eq!(encoding, PaneInfoEncoding::LegacyEscapedOctal);
        assert_eq!(info.current_command, "zsh");
        assert_eq!(info.cwd, "/tmp/user/project|pipe");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.width, 120);
        assert_eq!(info.height, 40);
    }

    #[test]
    fn test_run_tmux_command_sync_result_drains_large_stdout() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "yes x | head -c 262144".to_string(),
        ];
        let output = run_tmux_command_sync_result(&argv).expect("large stdout should not deadlock");
        assert_eq!(output.len(), 256 * 1024);
        assert!(output.starts_with("x\nx\n"));
    }

    // --- list_sessions_dashboard_command ---

    #[test]
    fn test_list_sessions_dashboard_command_local() {
        let cmd = list_sessions_dashboard_command(&TmuxTarget::Local);
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "list-sessions",
                "-F",
                "#{session_name}:#{session_created}:#{session_attached}:#{session_windows}"
            ]
        );
    }

    #[test]
    fn test_list_sessions_dashboard_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = list_sessions_dashboard_command(&target);
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "user@host",
                "tmux",
                "list-sessions",
                "-F",
                "'#{session_name}:#{session_created}:#{session_attached}:#{session_windows}'"
            ]
        );
    }

    #[test]
    fn tmux_remote_quoting_quotes_hash_format_strings() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = pane_current_command(&target, "my-session");
        assert!(cmd.contains(&"'#{pane_current_command}'".to_string()));

        let remote_command = shell_join_for_ssh(&cmd[6..]);
        assert_eq!(
            remote_command,
            "tmux display-message -t my-session -p '#{pane_current_command}'"
        );
    }

    #[test]
    fn tmux_remote_quoting_pane_info_remote_argv_is_shell_safe() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = pane_info_command(&target, "my-session");
        let remote_command = shell_join_for_ssh(&cmd[6..]);

        assert_eq!(
            remote_command,
            "tmux display-message -t my-session -p '#{pane_current_command}__TAAROF_PANE_INFO_V1__#{pane_current_path}__TAAROF_PANE_INFO_V1__#{pane_pid}__TAAROF_PANE_INFO_V1__#{pane_width}__TAAROF_PANE_INFO_V1__#{pane_height}__TAAROF_PANE_INFO_V1__#{session_id}__TAAROF_PANE_INFO_V1__#{session_created}__TAAROF_PANE_INFO_V1__#{@taarof-continuity-id}'"
        );
        assert!(!remote_command.contains(LEGACY_PANE_INFO_SEPARATOR));
    }

    #[test]
    fn tmux_remote_quoting_list_sessions_dashboard_remote_argv_is_shell_safe() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = list_sessions_dashboard_command(&target);
        let remote_command = shell_join_for_ssh(&cmd[6..]);

        assert_eq!(
            remote_command,
            "tmux list-sessions -F '#{session_name}:#{session_created}:#{session_attached}:#{session_windows}'"
        );
    }

    // --- parse_list_sessions_tuples ---

    #[test]
    fn test_parse_list_sessions_tuples_basic() {
        let output = "taarof--default--t3--0:1711800000:1:1\nother-session:1711700000:0:2\n";
        let sessions = parse_list_sessions_tuples(output).expect("valid output should parse");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].0, "taarof--default--t3--0");
        assert!(sessions[0].2);
        assert_eq!(sessions[1].0, "other-session");
        assert!(!sessions[1].2);
        assert_eq!(sessions[1].3, 2);
    }

    #[test]
    fn test_parse_list_sessions_tuples_empty() {
        let sessions = parse_list_sessions_tuples("").expect("empty output is authoritative");
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_parse_list_sessions_tuples_rejects_invalid_fields() {
        let output = "good:1711800000:1:1\nbadline\nalso:bad:line";
        assert!(parse_list_sessions_tuples(output).is_err());
    }

    #[test]
    fn complete_list_sessions_parser_rejects_partially_malformed_output() {
        let output = "good:1711800000:1:1\nbadline\nalso-good:1711800001:0:2\n";

        let error = parse_list_sessions_tuples(output)
            .expect_err("one malformed line must invalidate the entire target snapshot");

        assert_eq!(error, "malformed tmux list-sessions output at line 2");
    }

    // --- is_shell_command ---

    #[test]
    fn test_is_shell_command() {
        for shell in &["bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh"] {
            assert!(is_shell_command(shell), "{} should be a shell", shell);
        }
        assert!(!is_shell_command("vim"));
        assert!(!is_shell_command("python3"));
        assert!(!is_shell_command("node"));
        assert!(!is_shell_command(""));
    }

    // --- TmuxTarget::ssh_target_string ---

    #[test]
    fn test_ssh_target_string_local() {
        assert_eq!(TmuxTarget::Local.ssh_target_string(), None);
    }

    #[test]
    fn test_ssh_target_string_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@example.com".to_string(),
        };
        assert_eq!(
            target.ssh_target_string(),
            Some("user@example.com".to_string())
        );
    }

    // --- capture_pane_command ---

    #[test]
    fn test_capture_pane_command_local() {
        let cmd = capture_pane_command(&TmuxTarget::Local, "my-session", 10);
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "capture-pane",
                "-p",
                "-J",
                "-t",
                "my-session",
                "-l",
                "10"
            ]
        );
    }

    #[test]
    fn test_capture_pane_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = capture_pane_command(&target, "my-session", 50);
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.contains(&"capture-pane".to_string()));
        assert!(cmd.contains(&"50".to_string()));
    }

    #[test]
    fn test_capture_pane_ansi_command_local() {
        let cmd = capture_pane_ansi_command(&TmuxTarget::Local, "my-session");
        assert_eq!(
            cmd,
            vec!["tmux", "capture-pane", "-e", "-p", "-J", "-t", "my-session"]
        );
    }

    #[test]
    fn test_capture_pane_ansi_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = capture_pane_ansi_command(&target, "my-session");
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.contains(&"capture-pane".to_string()));
        assert!(cmd.contains(&"-e".to_string()));
    }

    #[test]
    fn test_capture_pane_text_command_local() {
        let cmd = capture_pane_text_command(&TmuxTarget::Local, "my-session");
        assert_eq!(
            cmd,
            vec!["tmux", "capture-pane", "-p", "-J", "-t", "my-session"]
        );
    }

    #[test]
    fn test_resize_pane_command_local() {
        let cmd = resize_pane_command(&TmuxTarget::Local, "my-session", 120, 40);
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "resize-pane",
                "-t",
                "my-session",
                "-x",
                "120",
                "-y",
                "40",
            ]
        );
    }

    #[test]
    fn test_send_keys_command_local_preserves_payload_argument() {
        let cmd = send_keys_command(&TmuxTarget::Local, "my-session", "\u{3}abc\r\n");
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "send-keys",
                "-t",
                "my-session",
                "-l",
                "\u{3}abc\r\n",
            ]
        );
    }

    // --- has_session_command_batch ---

    #[test]
    fn test_has_session_command_batch_local() {
        let cmd = has_session_command_batch(&TmuxTarget::Local, "my-session");
        assert_eq!(cmd, vec!["tmux", "has-session", "-t", "my-session"]);
    }

    #[test]
    fn test_has_session_command_batch_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = has_session_command_batch(&target, "my-session");
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "user@host",
                "tmux",
                "has-session",
                "-t",
                "my-session",
            ]
        );
    }

    // --- pane_current_command ---

    #[test]
    fn test_pane_current_command_local() {
        let cmd = pane_current_command(&TmuxTarget::Local, "my-session");
        assert_eq!(
            cmd,
            vec![
                "tmux",
                "display-message",
                "-t",
                "my-session",
                "-p",
                "#{pane_current_command}",
            ]
        );
    }

    #[test]
    fn test_pane_current_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = pane_current_command(&target, "my-session");
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.contains(&"display-message".to_string()));
        assert!(cmd.contains(&"'#{pane_current_command}'".to_string()));
    }

    // --- attach_command ---

    #[test]
    fn test_attach_command_local() {
        let cmd = attach_command(&TmuxTarget::Local, "my-session");
        assert_eq!(cmd, vec!["tmux", "attach-session", "-t", "my-session"]);
    }

    #[test]
    fn test_attach_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = attach_command(&target, "my-session");
        assert_eq!(
            cmd,
            vec![
                "ssh",
                "-t",
                "user@host",
                "tmux",
                "attach-session",
                "-t",
                "my-session"
            ]
        );
    }

    #[test]
    fn test_version_command_local() {
        let cmd = version_command(&TmuxTarget::Local);
        assert_eq!(cmd, vec!["tmux", "-V"]);
    }

    #[test]
    fn test_version_command_remote() {
        let target = TmuxTarget::Remote {
            ssh_target: "user@host".to_string(),
        };
        let cmd = version_command(&target);
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.contains(&"BatchMode=yes".to_string()));
        assert!(cmd.contains(&"tmux".to_string()));
        assert!(cmd.contains(&"-V".to_string()));
    }

    // --- kill_session_command (new tests for noninteractive wrapper) ---

    #[test]
    fn test_kill_session_command_local() {
        let cmd = kill_session_command(&TmuxTarget::Local, "taarof--test--t1--0");
        assert_eq!(
            cmd,
            vec!["tmux", "kill-session", "-t", "taarof--test--t1--0"]
        );
    }

    #[test]
    fn test_kill_session_command_remote() {
        let cmd = kill_session_command(
            &TmuxTarget::Remote {
                ssh_target: "user@host".into(),
            },
            "taarof--test--t1--0",
        );
        assert_eq!(cmd[0], "ssh");
        assert!(cmd.contains(&"kill-session".to_string()));
    }
}
