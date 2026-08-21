//! Remote SSH mise command helpers.

use super::*;

pub(super) fn ssh_supports_remote_exec(ssh_argv: &[String]) -> bool {
    ssh_argv
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "ssh" | "autossh"))
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn remote_mise_command(cwd: &str, args: &[&str], exec: bool) -> String {
    let quoted_args = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let binary_invocation = if exec {
        "exec \"$MISE_BIN\""
    } else {
        "\"$MISE_BIN\""
    };

    format!(
        "MISE_BIN=$(command -v mise || true); \
if [ -z \"$MISE_BIN\" ] && [ -x \"$HOME/.local/bin/mise\" ]; then MISE_BIN=\"$HOME/.local/bin/mise\"; fi; \
[ -n \"$MISE_BIN\" ] || exit 127; \
cd {} && {} {}",
        shell_quote(cwd),
        binary_invocation,
        quoted_args,
    )
}

pub(super) fn remote_mise_shell_command(cwd: &str, args: &[&str]) -> String {
    remote_mise_command(cwd, args, true)
}

#[cfg(test)]
pub(super) fn remote_mise_run_command(cwd: &str, args: &[&str]) -> String {
    remote_mise_command(cwd, args, false)
}

pub(super) fn remote_shell_respawn_command(cwd: &str) -> String {
    format!(
        "cd {} && if [ -n \"$SHELL\" ] && [ -x \"$SHELL\" ]; then exec \"$SHELL\"; else exec /bin/bash; fi",
        shell_quote(cwd),
    )
}

/// Find the SSH destination (first non-option positional arg) index in argv.
/// Needed because ssh_argv may have been captured from /proc/<pid>/cmdline of a
/// running ssh process and carry a trailing positional remote command — if we
/// append our own remote_command on top, SSH joins both with spaces and the
/// captured payload (e.g. `cd ... && exec "$SHELL"`) preempts our probe.
pub(super) fn ssh_destination_index(argv: &[String]) -> Option<usize> {
    // ssh(1) short flags that consume the following arg as their value.
    const VALUE_FLAGS: &[&str] = &[
        "-B", "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o",
        "-p", "-Q", "-R", "-S", "-W", "-w",
    ];

    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() {
                // bare "-" is not an SSH option; treat as destination
                return Some(i);
            }
            if arg == "--" {
                // end of options; next positional is destination
                return argv.get(i + 1).map(|_| i + 1);
            }
            // Short flag exactly like "-p", "-o" with value in next argv slot.
            if arg.len() == 2 && VALUE_FLAGS.contains(&arg.as_str()) {
                i += 2;
                continue;
            }
            // Long option (e.g. "--verbose") or bare/clustered short flag
            // (e.g. "-tt", "-q", "-p2222", "-i/path/key"). The value is either
            // attached or unused — either way, advance by one.
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

pub(super) fn ssh_command_with_remote_exec(
    ssh_argv: &[String],
    remote_command: String,
    force_tty: bool,
    extra_ssh_args: &[&str],
) -> Option<Vec<String>> {
    if ssh_argv.is_empty() || !ssh_supports_remote_exec(ssh_argv) {
        return None;
    }

    let destination_idx = ssh_destination_index(ssh_argv)?;
    let head = &ssh_argv[..=destination_idx];

    let mut argv = Vec::with_capacity(head.len() + extra_ssh_args.len() + 2);
    argv.push(head[0].clone());
    if force_tty
        && !head
            .iter()
            .skip(1)
            .any(|arg| arg == "-t" || arg == "-tt" || arg == "-T")
    {
        argv.push("-tt".into());
    }
    argv.extend(extra_ssh_args.iter().map(|arg| arg.to_string()));
    argv.extend(head.iter().skip(1).cloned());
    argv.push(remote_command);
    Some(argv)
}
