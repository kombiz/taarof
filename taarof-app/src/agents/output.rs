//! Output-signal scanner for the agent sidecar.
//!
//! This module owns the text-normalization helpers, the line-based
//! activity parser, and the `scan_output_signal` entry point that turns
//! captured terminal text into a structured `ScannedActivity`.

use std::path::Path;

use crate::workspace::AgentActivityState;
pub fn summarize_activity_text(text: &str) -> String {
    truncate_visible_chars(&normalize_activity_text(text), 24)
}

/// Label a notification with the agent it came from so multi-agent tabs
/// name the specific agent and pane, e.g. `claude (pane 2): waiting for input`.
/// Pass `pane_id: Some(..)` only when the pane matters for disambiguation.
pub fn format_agent_notification(
    source: Option<&str>,
    pane_id: Option<u32>,
    summary: &str,
) -> String {
    match (source, pane_id) {
        (Some(agent), Some(pane)) => format!("{agent} (pane {pane}): {summary}"),
        (Some(agent), None) => format!("{agent}: {summary}"),
        (None, Some(pane)) => format!("pane {pane}: {summary}"),
        (None, None) => summary.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedActivity {
    pub state: AgentActivityState,
    pub summary: String,
}

#[cfg(test)]
pub fn scan_output_activity(text: &str) -> Option<String> {
    scan_output_signal(text).map(|activity| activity.summary)
}

pub fn scan_output_signal(text: &str) -> Option<ScannedActivity> {
    const MAX_RECENT_LINES: usize = 48;

    text.lines()
        .rev()
        .take(MAX_RECENT_LINES)
        .filter_map(parse_activity_line)
        .next()
        .map(|candidate| {
            let summary = summarize_activity_text(&candidate);
            let state = if candidate.eq_ignore_ascii_case("waiting for input") {
                AgentActivityState::WaitingInput
            } else {
                AgentActivityState::Running
            };
            ScannedActivity { state, summary }
        })
}

fn normalize_activity_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return String::new();
    }

    // Data-driven prefix dispatch. Order is significant: more-specific prefixes
    // (e.g. "WebFetch ") must appear before shorter ones that could shadow them.
    // Each entry is (prefix, renderer): renderer receives the stripped tail and
    // returns the final display string.
    type Renderer = fn(&str) -> String;
    static PREFIXES: &[(&str, Renderer)] = &[
        ("Read ", |t| format!("reading {}", summarize_path(t))),
        ("Write ", |t| format!("editing {}", summarize_path(t))),
        ("Edit ", |t| format!("editing {}", summarize_path(t))),
        ("Update ", |t| format!("editing {}", summarize_path(t))),
        ("Bash:", |t| format!("running {}", t.trim())),
        ("Shell:", |t| format!("running {}", t.trim())),
        ("Search ", |t| {
            format!("searching {}", summarize_fragment(t))
        }),
        ("Search:", |t| {
            format!("searching {}", summarize_fragment(t))
        }),
        ("Grep ", |t| format!("searching {}", summarize_fragment(t))),
        ("Glob ", |t| format!("scanning {}", summarize_fragment(t))),
        ("WebFetch ", |t| {
            format!("fetching {}", summarize_fragment(t))
        }),
        ("Fetch ", |t| format!("fetching {}", summarize_fragment(t))),
        ("Fetch:", |t| format!("fetching {}", summarize_fragment(t))),
        ("Agent ", |t| {
            format!("delegating {}", summarize_fragment(t))
        }),
    ];

    if let Some(result) = PREFIXES
        .iter()
        .find_map(|(prefix, render)| strip_prefix_ignore_ascii_case(&collapsed, prefix).map(render))
    {
        return result;
    }

    let lowercase = collapsed.to_ascii_lowercase();
    if lowercase.contains("waiting") && lowercase.contains("input") {
        return "waiting for input".to_string();
    }

    lower_first_char(&collapsed)
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| value[prefix.len()..].trim())
}

fn summarize_path(raw: &str) -> String {
    let cleaned = raw.trim().trim_matches(['"', '\'', '`']);
    let path = Path::new(cleaned);
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned.to_string()
    }
}

fn summarize_fragment(raw: &str) -> String {
    let cleaned = raw.trim().trim_matches(['"', '\'', '`']);
    if cleaned.is_empty() {
        "task".to_string()
    } else {
        cleaned.to_string()
    }
}

fn lower_first_char(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut lowered = first.to_lowercase().collect::<String>();
            lowered.push_str(chars.as_str());
            lowered
        }
        None => String::new(),
    }
}

fn truncate_visible_chars(text: &str, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }

    let chars: Vec<char> = text.chars().collect();
    let keep = max_chars.saturating_sub(4);
    let mut truncated: String = chars.iter().take(keep).collect();
    if chars.get(keep).is_some_and(|next| !next.is_whitespace()) {
        if let Some(last_space) = truncated.rfind(' ') {
            if last_space >= keep / 2 {
                truncated.truncate(last_space);
            }
        }
    }

    let truncated = truncated.trim_end();
    if truncated
        .chars()
        .last()
        .is_some_and(|c| c.is_alphanumeric())
    {
        format!("{truncated} ...")
    } else {
        format!("{truncated}...")
    }
}

fn parse_activity_line(line: &str) -> Option<String> {
    let line = strip_activity_prefix(line);
    if line.is_empty() {
        return None;
    }

    let lowercase = line.to_ascii_lowercase();
    if lowercase.contains("waiting for user input")
        || lowercase.contains("waiting for input")
        || lowercase.contains("needs your permission")
        || lowercase.contains("permission needed")
    {
        return Some("waiting for input".to_string());
    }
    if lowercase.contains("enter to select")
        && (lowercase.contains("esc to cancel") || lowercase.contains("navigate"))
    {
        return Some("waiting for input".to_string());
    }

    // Data-driven label dispatch. Each entry is (label, canonical_prefix):
    // extract_activity_argument handles all suffix forms (" ", ":", "(...)").
    // Order matters: "WebFetch" before "Fetch" to avoid the shorter label
    // consuming the longer keyword.
    static LABELS: &[(&str, &str)] = &[
        ("Read", "Read"),
        ("Write", "Write"),
        ("Edit", "Edit"),
        ("Update", "Update"),
        ("Bash", "Bash:"),
        ("Shell", "Shell:"),
        ("Grep", "Search"),
        ("Glob", "Glob"),
        ("WebFetch", "Fetch"),
        ("Search", "Search"),
        ("Fetch", "Fetch"),
        ("Agent", "Agent"),
    ];

    if let Some(result) = LABELS.iter().find_map(|(label, canonical)| {
        extract_activity_argument(line, label).map(|arg| format!("{canonical} {arg}"))
    }) {
        return Some(result);
    }

    None
}

fn strip_activity_prefix(line: &str) -> &str {
    line.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '-' | '*' | '•' | '●' | '⏺' | '>' | '|')
    })
    .trim()
}

fn extract_activity_argument(line: &str, label: &str) -> Option<String> {
    for suffix in [" ", ":"] {
        let prefix = format!("{label}{suffix}");
        if let Some(value) = strip_prefix_ignore_ascii_case(line, &prefix) {
            let value = value.trim().trim_matches(['"', '\'', '`']);
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    let paren_prefix = format!("{label}(");
    if let Some(value) = strip_prefix_ignore_ascii_case(line, &paren_prefix) {
        let value = value
            .split(')')
            .next()
            .unwrap_or(value)
            .trim()
            .trim_matches(['"', '\'', '`']);
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }

    None
}

#[cfg(test)]
mod notification_tests {
    use super::format_agent_notification;

    #[test]
    fn format_agent_notification_labels_agent_and_pane() {
        assert_eq!(
            format_agent_notification(Some("claude"), Some(2), "waiting for input"),
            "claude (pane 2): waiting for input"
        );
        assert_eq!(
            format_agent_notification(Some("codex"), None, "done"),
            "codex: done"
        );
        assert_eq!(
            format_agent_notification(None, Some(1), "errored"),
            "pane 1: errored"
        );
        assert_eq!(format_agent_notification(None, None, "done"), "done");
    }
}
