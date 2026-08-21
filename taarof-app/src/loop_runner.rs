//! Config-driven dispatch to an external "loop runner" CLI.
//!
//! taarof never hardcodes a specific tool: the command lines come from the
//! `[loop_runner]` config section (see [`crate::config::LoopRunnerConfig`]) as
//! templates with `{repo}` / `{issue}` / `{pr}` / `{loop}` placeholders. This
//! module holds the pure, unit-tested pieces — placeholder substitution, argv
//! splitting, the live-tab command wrapper that tees the runner's JSON to a
//! result file, and the parse of that result into a user-facing toast — so the
//! GTK panel code stays a thin shell around testable logic.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Which PR-review action a dispatch represents, selecting the merging vs.
/// non-merging loop. (Task builds don't go through this — they always use the
/// `dev-loop` command directly.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopRunnerAction {
    /// `pr-loop` with the non-merging review loop.
    ReviewOnly,
    /// `pr-loop` with the merge-capable loop.
    ReviewAndMerge,
}

/// Substitute `{repo}` / `{issue}` / `{pr}` / `{loop}` placeholders in a command
/// template. Unknown placeholders are left untouched; absent keys are simply not
/// substituted. Pure and order-independent.
pub fn substitute_placeholders(template: &str, values: &BTreeMap<&str, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

/// Build the substituted `dev-loop` command line from a template.
///
/// Every substituted value is shell-quoted: the resulting line is embedded in a
/// `bash -lc` script by [`live_tab_argv`], so values that originate from
/// `.plan/tasks.json` (the task id) or a checkout path MUST NOT be able to break
/// out of their argument. Single-quoting makes each value one inert literal.
pub fn dev_command_line(template: &str, repo: &str, issue: &str, loop_name: &str) -> String {
    let mut values = BTreeMap::new();
    values.insert("repo", shell_single_quote(repo));
    values.insert("issue", shell_single_quote(issue));
    values.insert("loop", shell_single_quote(loop_name));
    substitute_placeholders(template, &values)
}

/// Build the substituted `pr-loop` command line from a template. Like
/// [`dev_command_line`], every substituted value is shell-quoted before it is
/// embedded in the `bash -lc` script.
pub fn pr_command_line(template: &str, repo: &str, pr: &str, loop_name: &str) -> String {
    let mut values = BTreeMap::new();
    values.insert("repo", shell_single_quote(repo));
    values.insert("pr", shell_single_quote(pr));
    values.insert("loop", shell_single_quote(loop_name));
    substitute_placeholders(template, &values)
}

/// Whether a task/issue id is safe to dispatch: a strict allowlist of
/// `[A-Za-z0-9._-]`. Although [`dev_command_line`] already shell-quotes every
/// value, the dispatch path rejects ids outside this set up front — defense in
/// depth, and a clear error for a malformed/hostile `.plan/tasks.json` id rather
/// than passing a strange (but quoted) blob to the runner.
pub fn is_safe_issue_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Select the loop name for a PR action: the non-merging `review_loop` for
/// "Review only", the merge-capable `merge_loop` for "Review & merge".
pub fn pr_loop_name<'a>(
    action: LoopRunnerAction,
    review_loop: &'a str,
    merge_loop: &'a str,
) -> &'a str {
    match action {
        LoopRunnerAction::ReviewOnly => review_loop,
        LoopRunnerAction::ReviewAndMerge => merge_loop,
    }
}

/// Split a command line into argv, honoring single quotes, double quotes, and
/// backslash escapes (POSIX-ish). Returns `None` for an empty/blank command.
///
/// This is intentionally small: it covers the quoting the panel needs to show a
/// faithful argv preview and to detect an empty template, without pulling in a
/// shell-words dependency that would complicate the public export.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if has_token {
                    argv.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' => {
                has_token = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    current.push(inner);
                }
            }
            '"' => {
                has_token = true;
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            if let Some(&next) = chars.peek() {
                                if matches!(next, '"' | '\\' | '$' | '`') {
                                    current.push(next);
                                    chars.next();
                                    continue;
                                }
                            }
                            current.push('\\');
                        }
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                has_token = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            other => {
                has_token = true;
                current.push(other);
            }
        }
    }

    if has_token {
        argv.push(current);
    }

    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Path of the per-dispatch JSON result file under `$XDG_RUNTIME_DIR`.
///
/// `kind` is a short tag (`task`/`pr`) and `id` the task or PR identifier; the
/// file name is sanitized so an arbitrary id can't escape the runtime dir.
pub fn result_file_path(runtime_dir: &str, kind: &str, id: &str) -> PathBuf {
    let safe_id: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(runtime_dir).join(format!("taarof-loop-{kind}-{safe_id}.json"))
}

/// Wrap a substituted runner command so its single-line JSON result is captured
/// to `result_path` while the agent run is still visible live in the tab.
///
/// The runner prints exactly one JSON object on stdout (agent chatter goes to its
/// own audit dir), so `tee` writes a clean result file. `set -o pipefail` keeps
/// the tab's exit status meaningful, and the result file is removed up-front so a
/// stale file from a previous run can't be misread as this run's outcome.
pub fn live_tab_argv(command: &str, result_path: &str) -> Vec<String> {
    let script = format!(
        "rm -f {result} 2>/dev/null; set -o pipefail; {command} | tee {result}",
        result = shell_single_quote(result_path),
        command = command,
    );
    vec!["bash".to_string(), "-lc".to_string(), script]
}

/// Gate a task-bound runner on its private reporting context. The pane is
/// created before taarof knows its stable identity, so the lightweight shell
/// waits until the dispatcher has bound and validated the pane, sources the
/// 0600 export file, removes it, and only then starts the real runner.
pub fn task_bound_live_tab_argv(
    command: &str,
    result_path: &str,
    context_path: &str,
) -> Vec<String> {
    let context = shell_single_quote(context_path);
    let script = format!(
        "_taarof_context={context}; trap 'rm -f -- \"$_taarof_context\"' EXIT; for ((_taarof_wait=0; _taarof_wait<200; _taarof_wait++)); do [ -f \"$_taarof_context\" ] && break; sleep 0.05; done; if [ ! -f \"$_taarof_context\" ]; then echo 'taarof: task reporting context was not supplied' >&2; exit 70; fi; . \"$_taarof_context\"; rm -f -- \"$_taarof_context\"; rm -f {result} 2>/dev/null; set -o pipefail; {command} | tee {result}",
        result = shell_single_quote(result_path),
    );
    vec!["bash".to_string(), "-lc".to_string(), script]
}

/// Quote a string for safe inclusion in a POSIX shell command.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A toast classification for a parsed loop-runner result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopRunnerToast {
    Success(String),
    Info(String),
    Error(String),
}

/// Parse a `dev-loop` JSON result into a toast.
///
/// Success (`status == "done"`) surfaces the `pr_url`; any other status surfaces
/// the status as a failure reason. Unparseable output is an error toast.
pub fn dev_result_toast(json: &str) -> LoopRunnerToast {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return LoopRunnerToast::Error("Loop runner produced no readable result".to_string());
    };
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match status {
        "done" => match value.get("pr_url").and_then(|v| v.as_str()) {
            Some(url) if !url.is_empty() => LoopRunnerToast::Success(format!("PR opened: {url}")),
            _ => LoopRunnerToast::Success("Task built (no PR URL reported)".to_string()),
        },
        "dry-run" => LoopRunnerToast::Info("Dry run complete — no changes made".to_string()),
        other => LoopRunnerToast::Error(format!("Loop runner finished with status: {other}")),
    }
}

/// Parse a `pr-loop` JSON result into a toast, mapping the documented statuses:
/// `merged` → success, `reviewed` → "review posted" (non-merging loop),
/// `changes_requested` → "comment posted", `checks_not_green` → "checks not green".
pub fn pr_result_toast(json: &str) -> LoopRunnerToast {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return LoopRunnerToast::Error("Loop runner produced no readable result".to_string());
    };
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match status {
        "merged" => LoopRunnerToast::Success("PR merged".to_string()),
        "reviewed" => LoopRunnerToast::Success("Review posted on the PR — not merged".to_string()),
        "changes_requested" => {
            LoopRunnerToast::Info("Changes requested — comment posted on the PR".to_string())
        }
        "checks_not_green" => {
            LoopRunnerToast::Error("Checks not green — PR was not merged".to_string())
        }
        "dry-run" => LoopRunnerToast::Info("Dry run complete — no review effects".to_string()),
        other => LoopRunnerToast::Error(format!("Loop runner finished with status: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_runner_dev_command_template_substitutes_repo_and_issue() {
        let cmd = dev_command_line(
            "loop-runner dev-loop --repo {repo} --issue {issue}",
            "/tmp/user/proj",
            "EXAMPLE-57",
            "dev",
        );
        // Substituted values are shell-quoted (the line is run under `bash -lc`).
        assert_eq!(
            cmd,
            "loop-runner dev-loop --repo '/tmp/user/proj' --issue 'EXAMPLE-57'"
        );
        // {loop} is available even when the default template omits it.
        let with_loop = dev_command_line(
            "runner build --repo {repo} --issue {issue} --loop {loop}",
            "/r",
            "T-1",
            "fast",
        );
        assert_eq!(
            with_loop,
            "runner build --repo '/r' --issue 'T-1' --loop 'fast'"
        );
    }

    #[test]
    fn loop_runner_pr_command_template_substitutes_repo_and_pr() {
        let cmd = pr_command_line(
            "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}",
            "/tmp/user/proj",
            "208",
            "pr-review",
        );
        assert_eq!(
            cmd,
            "loop-runner pr-loop --repo '/tmp/user/proj' --pr '208' --loop 'pr-review'"
        );
    }

    #[test]
    fn pr_loop_review_only_selects_non_merging_loop() {
        let review = "pr-review-readonly";
        let merge = "pr-review";
        assert_eq!(
            pr_loop_name(LoopRunnerAction::ReviewOnly, review, merge),
            "pr-review-readonly"
        );
        assert_eq!(
            pr_loop_name(LoopRunnerAction::ReviewAndMerge, review, merge),
            "pr-review"
        );
        // And the full command uses the non-merging loop name.
        let cmd = pr_command_line(
            "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}",
            "/r",
            "208",
            pr_loop_name(LoopRunnerAction::ReviewOnly, review, merge),
        );
        assert!(cmd.contains("--loop 'pr-review-readonly'"));
        assert!(!cmd.contains("--loop 'pr-review'"));
    }

    #[test]
    fn dev_command_neutralizes_shell_metacharacters_in_issue_id() {
        // A hostile task id from .plan/tasks.json must not break out of its
        // argument when the line is embedded in `bash -lc`.
        let hostile = "EXAMPLE-57; touch /tmp/INJECTED";
        let cmd = dev_command_line(
            "loop-runner dev-loop --repo {repo} --issue {issue}",
            "/r",
            hostile,
            "dev",
        );
        // The whole id, semicolon and all, is one single-quoted literal.
        assert!(cmd.contains("--issue 'EXAMPLE-57; touch /tmp/INJECTED'"));
        // Embedding it in the live-tab script keeps it quoted (inert).
        let script = live_tab_argv(&cmd, "/run/r.json").pop().unwrap();
        assert!(script.contains("'EXAMPLE-57; touch /tmp/INJECTED'"));
        // An embedded single quote is escaped via the POSIX '\'' trick, so even
        // that cannot terminate the quoting early.
        let tricky = dev_command_line("r --issue {issue}", "/x", "a'b", "dev");
        assert_eq!(tricky, r#"r --issue 'a'\''b'"#);
        // And the dispatch-site allowlist rejects such ids outright.
        assert!(!is_safe_issue_id(hostile));
        assert!(!is_safe_issue_id("a'b"));
        assert!(is_safe_issue_id("EXAMPLE-57"));
        assert!(is_safe_issue_id("feat.2_x-1"));
    }

    #[test]
    fn pr_loop_result_status_maps_to_expected_toast_message() {
        assert_eq!(
            pr_result_toast(r#"{"status":"merged","pr":"208"}"#),
            LoopRunnerToast::Success("PR merged".to_string())
        );
        assert_eq!(
            pr_result_toast(r#"{"status":"reviewed","pr":"208"}"#),
            LoopRunnerToast::Success("Review posted on the PR — not merged".to_string())
        );
        assert_eq!(
            pr_result_toast(r#"{"status":"changes_requested"}"#),
            LoopRunnerToast::Info("Changes requested — comment posted on the PR".to_string())
        );
        assert_eq!(
            pr_result_toast(r#"{"status":"checks_not_green"}"#),
            LoopRunnerToast::Error("Checks not green — PR was not merged".to_string())
        );
        // Garbage / empty output is an error toast, not a panic.
        assert!(matches!(
            pr_result_toast("not json"),
            LoopRunnerToast::Error(_)
        ));
    }

    #[test]
    fn dev_result_status_maps_to_expected_toast_message() {
        assert_eq!(
            dev_result_toast(r#"{"status":"done","pr_url":"https://example/pr/1"}"#),
            LoopRunnerToast::Success("PR opened: https://example/pr/1".to_string())
        );
        assert!(matches!(
            dev_result_toast(r#"{"status":"error"}"#),
            LoopRunnerToast::Error(_)
        ));
        assert!(matches!(dev_result_toast(""), LoopRunnerToast::Error(_)));
    }

    #[test]
    fn split_command_handles_quotes_and_escapes() {
        assert_eq!(
            split_command("loop-runner dev-loop --repo /r --issue EXAMPLE-57"),
            Some(vec![
                "loop-runner".into(),
                "dev-loop".into(),
                "--repo".into(),
                "/r".into(),
                "--issue".into(),
                "EXAMPLE-57".into(),
            ])
        );
        // A quoted path with spaces stays one argument.
        assert_eq!(
            split_command(r#"runner --repo "/tmp/user repo" --issue T-1"#),
            Some(vec![
                "runner".into(),
                "--repo".into(),
                "/tmp/user repo".into(),
                "--issue".into(),
                "T-1".into(),
            ])
        );
        assert_eq!(split_command("   "), None);
    }

    #[test]
    fn live_tab_argv_tees_result_and_clears_stale_file() {
        let argv = live_tab_argv(
            "loop-runner dev-loop --repo /r --issue T-1",
            "/run/taarof-loop-task-T-1.json",
        );
        assert_eq!(argv[0], "bash");
        assert_eq!(argv[1], "-lc");
        let script = &argv[2];
        assert!(script.contains("rm -f '/run/taarof-loop-task-T-1.json'"));
        assert!(script.contains("set -o pipefail"));
        assert!(script.contains("loop-runner dev-loop --repo /r --issue T-1"));
        assert!(script.contains("tee '/run/taarof-loop-task-T-1.json'"));
    }

    #[test]
    fn task_bound_wrapper_loads_context_before_runner_and_removes_private_file() {
        let root = std::env::temp_dir().join(format!(
            "taarof-loop-context-{}-quote's",
            std::process::id()
        ));
        let result = root.join("result.json");
        let context_path = crate::work_reporting::context_file_path(&root, "EXAMPLE-112");
        let context = crate::work_reporting::WorkContext {
            session: "default".into(),
            workspace_origin: "workspace-fixture".into(),
            tab_origin: "tab-fixture".into(),
            pane_origin: "pane-fixture".into(),
            task_id: "EXAMPLE-112".into(),
            task_title: "Explicit reporting".into(),
            checkout_root: "/repo".into(),
            binding_token: crate::task_binding::new_reporting_token(),
            tab_id: 7,
            pane_id: 3,
        };
        crate::work_reporting::write_context_file(&context_path, &context).unwrap();
        let command = "printf '{\"task\":\"%s\",\"title\":\"%s\",\"has_instructions\":%s}\\n' \"$TAAROF_WORK_TASK_ID\" \"$TAAROF_WORK_TASK_TITLE\" \"$([ -n \"$TAAROF_WORK_INSTRUCTIONS\" ] && echo true || echo false)\"";
        let argv = task_bound_live_tab_argv(
            command,
            &result.to_string_lossy(),
            &context_path.to_string_lossy(),
        );
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "{\"task\":\"EXAMPLE-112\",\"title\":\"Explicit reporting\",\"has_instructions\":true}\n"
        );
        assert!(!context_path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn result_file_path_sanitizes_id() {
        let path = result_file_path("/run/user/1000", "task", "EXAMPLE-57");
        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/taarof-loop-task-EXAMPLE-57.json")
        );
        // A hostile id can't traverse out of the runtime dir.
        let traversal = result_file_path("/run/user/1000", "pr", "../../etc/passwd");
        assert_eq!(
            traversal,
            PathBuf::from("/run/user/1000/taarof-loop-pr-______etc_passwd.json")
        );
    }
}
