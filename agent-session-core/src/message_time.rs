//! Bounded, metadata-only extraction. File mtime is never a message timestamp.
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const TAIL_BYTES: u64 = 1024 * 1024;

pub(crate) fn last_user_message(path: &Path, provider: &str) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let bytes = if start > 0 {
        // The first record may begin outside our budget.
        &bytes[bytes.iter().position(|b| *b == b'\n')? + 1..]
    } else {
        &bytes[..]
    };
    for line in bytes.rsplit(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // An incomplete or malformed newest record could itself be a message.
        let value: Value = serde_json::from_slice(line).ok()?;
        if is_user_message(&value, provider) {
            return timestamp(value.get("timestamp")?);
        }
    }
    None
}

fn timestamp(value: &Value) -> Option<u64> {
    let millis = if let Some(text) = value.as_str() {
        u64::try_from(
            chrono::DateTime::parse_from_rfc3339(text)
                .ok()?
                .timestamp_millis(),
        )
        .ok()?
    } else {
        value.as_u64()?
    };
    // Reject values outside the representable range rather than sorting nonsense first.
    chrono::DateTime::from_timestamp_millis(i64::try_from(millis).ok()?)?;
    Some(millis)
}

fn is_user_message(value: &Value, provider: &str) -> bool {
    let kind = value.get("type").and_then(Value::as_str);
    match provider {
        "claude" => {
            kind == Some("user")
                && value.get("isMeta").and_then(Value::as_bool) != Some(true)
                && value.get("isCompactSummary").and_then(Value::as_bool) != Some(true)
                && has_text(value.pointer("/message/content"))
        }
        "codex" => {
            (kind == Some("event_msg")
                && value.pointer("/payload/type").and_then(Value::as_str) == Some("user_message"))
                || (kind == Some("response_item")
                    && value.pointer("/payload/type").and_then(Value::as_str) == Some("message")
                    && value.pointer("/payload/role").and_then(Value::as_str) == Some("user")
                    && has_text(value.pointer("/payload/content")))
        }
        "pi" => {
            kind == Some("message")
                && value.pointer("/message/role").and_then(Value::as_str) == Some("user")
                && has_text(value.pointer("/message/content"))
        }
        _ => false,
    }
}

fn has_text(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) => items.iter().any(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("text" | "input_text")
            )
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn provider_messages_exclude_tool_results_and_assistant_output() {
        assert!(is_user_message(
            &json!({"type":"user","message":{"content":"hi"}}),
            "claude"
        ));
        assert!(!is_user_message(
            &json!({"type":"user","message":{"content":[{"type":"tool_result"}]}}),
            "claude"
        ));
        assert!(!is_user_message(
            &json!({"type":"user","isMeta":true,"message":{"content":"system"}}),
            "claude"
        ));
        assert!(is_user_message(
            &json!({"type":"event_msg","payload":{"type":"user_message"}}),
            "codex"
        ));
        assert!(is_user_message(
            &json!({"type":"message","message":{"role":"user","content":"hi"}}),
            "pi"
        ));
        assert!(!is_user_message(
            &json!({"type":"message","message":{"role":"assistant","content":"reply"}}),
            "pi"
        ));
        assert_eq!(
            timestamp(&json!("2026-09-06T10:00:00-05:00")),
            timestamp(&json!("2026-09-06T15:00:00Z"))
        );
        assert_eq!(timestamp(&json!(u64::MAX)), None);
    }

    #[test]
    fn reads_latest_message_beyond_head_sample_and_never_substitutes_mtime() {
        let path = std::env::temp_dir().join(format!("agent-message-time-{}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        for _ in 0..100 {
            writeln!(file, "{{\"type\":\"other\"}}").unwrap();
        }
        writeln!(file, "{{\"type\":\"user\",\"timestamp\":\"2026-09-06T15:00:00Z\",\"message\":{{\"content\":\"fixture\"}}}}").unwrap();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"timestamp\":\"2026-09-06T16:00:00Z\"}}"
        )
        .unwrap();
        assert_eq!(
            last_user_message(&path, "claude"),
            timestamp(&json!("2026-09-06T15:00:00Z"))
        );
        writeln!(
            file,
            "{{\"type\":\"user\",\"message\":{{\"content\":\"no timestamp\"}}}}"
        )
        .unwrap();
        assert_eq!(last_user_message(&path, "claude"), None);
        file.write_all(&vec![b'x'; TAIL_BYTES as usize + 1])
            .unwrap();
        assert_eq!(last_user_message(&path, "claude"), None);
        std::fs::remove_file(path).unwrap();
    }
}
