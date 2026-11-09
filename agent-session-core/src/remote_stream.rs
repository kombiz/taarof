//! Version 1 of the observe-only NDJSON protocol. Each line is one event.
//!
//! Forward compatibility: readers ignore unknown fields, and decode a line
//! whose `type` they do not know as [`Event::Unknown`], which they skip.
use crate::{remote_enumeration::RemoteParseStats, AgentSessionRecord};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on one emitted line, newline included. A reader may reject a
/// longer line. A snapshot that would exceed it drops its oldest sessions and
/// counts them in [`Stats::sessions_omitted`].
pub const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello {
        protocol_version: u32,
        helper_version: String,
        /// The remote kernel hostname, for display only. Identity comes from
        /// the consumer's own SSH target.
        host: String,
    },
    /// A complete replacement of the session list, sent only when sessions or
    /// stats change. Records are unstamped: `host` and `resume_command` are
    /// always absent, and the consumer fills them from its own SSH target.
    Snapshot {
        sessions: Vec<AgentSessionRecord>,
        stats: Stats,
    },
    /// Liveness plus the latest host metrics. Metrics change on every sample,
    /// so they travel here at the heartbeat cadence, not in snapshots.
    Heartbeat {
        observed_at_unix_ms: u64,
        metrics: Metrics,
    },
    /// Any event type this reader does not know. Skip it.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    /// One-minute load average (divide by nproc for the app's CPU percentage).
    pub load: Option<f64>,
    pub mem: Option<Memory>,
    pub nproc: Option<u32>,
    /// None means tmux failed, timed out, or is unavailable; never a false zero.
    pub tmux_session_count: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub total_bytes: u64,
    /// MemTotal - MemAvailable, matching current procps free's used memory.
    pub used_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stats {
    #[serde(flatten)]
    pub records: RemoteParseStats,
    /// The aggregate shell output reached the reader's 8 MiB limit.
    pub output_truncated: bool,
    /// No partial record set is published on a failed or timed-out enumeration.
    pub enumeration_failed: bool,
    /// Oldest sessions left out so the line fits [`MAX_EVENT_LINE_BYTES`].
    #[serde(default)]
    pub sessions_omitted: usize,
}

/// Serialize one event as an NDJSON line (newline included).
pub fn event_line(event: &Event) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// A snapshot line no longer than [`MAX_EVENT_LINE_BYTES`]. Sessions arrive
/// newest first, so the oldest are dropped first.
///
/// Each record is sized once and the list is trimmed in one pass. The fixed
/// overhead is measured with the largest omitted count this call can report,
/// so the estimate never undercounts; at the exact limit it may drop one more
/// session than strictly needed.
pub fn snapshot_line(
    mut sessions: Vec<AgentSessionRecord>,
    mut stats: Stats,
) -> serde_json::Result<Vec<u8>> {
    let sizes = sessions
        .iter()
        .map(|session| serde_json::to_vec(session).map(|bytes| bytes.len()))
        .collect::<serde_json::Result<Vec<_>>>()?;
    let mut worst = stats.clone();
    worst.sessions_omitted += sessions.len();
    let overhead = event_line(&Event::Snapshot {
        sessions: vec![],
        stats: worst,
    })?
    .len();
    let mut keep = sessions.len();
    // Records plus the commas between them.
    let mut total = overhead + sizes.iter().sum::<usize>() + keep.saturating_sub(1);
    while keep > 0 && total > MAX_EVENT_LINE_BYTES {
        keep -= 1;
        total -= sizes[keep] + usize::from(keep > 0);
    }
    stats.sessions_omitted += sessions.len() - keep;
    sessions.truncate(keep);
    event_line(&Event::Snapshot { sessions, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(id: usize, title: String) -> AgentSessionRecord {
        AgentSessionRecord {
            agent: "codex".into(),
            session_id: format!("s{id}"),
            title,
            cwd: "/synthetic".into(),
            host: None,
            repo_root: None,
            started_at_unix_ms: None,
            updated_at_unix_ms: 1_000 - id as u64,
            last_user_message_at_unix_ms: None,
            status: "recent".into(),
            live_binding: None,
            resume_command: None,
            resume_unavailable_reason: None,
        }
    }

    #[test]
    fn version_one_field_names_and_round_trip() {
        let hello = Event::Hello {
            protocol_version: PROTOCOL_VERSION,
            helper_version: "0.1.0".into(),
            host: "fixture.ts".into(),
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            json!({
                "type":"hello", "protocol_version":1,
                "helper_version":"0.1.0", "host":"fixture.ts"
            })
        );
        let snapshot = Event::Snapshot {
            sessions: vec![],
            stats: Stats::default(),
        };
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "type":"snapshot", "sessions":[],
                "stats":{"dropped_lines":0,"unparsable_lines":0,"truncated_files":0,
                    "blocks_without_session":0,"stores_incomplete":0,"output_truncated":false,
                    "enumeration_failed":false,"sessions_omitted":0}
            })
        );
        let heartbeat = Event::Heartbeat {
            observed_at_unix_ms: 42,
            metrics: Metrics {
                load: Some(2.0),
                mem: Some(Memory {
                    total_bytes: 100,
                    used_bytes: 50,
                }),
                nproc: Some(4),
                tmux_session_count: Some(0),
            },
        };
        assert_eq!(
            serde_json::to_value(&heartbeat).unwrap(),
            json!({"type":"heartbeat", "observed_at_unix_ms":42,
                "metrics":{"load":2.0,"mem":{"total_bytes":100,"used_bytes":50},
                "nproc":4,"tmux_session_count":0}})
        );
        for event in [hello, snapshot, heartbeat] {
            let line = serde_json::to_string(&event).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
        }
    }

    #[test]
    fn unknown_types_and_fields_are_tolerated() {
        // An unknown type (including a would-be control message) decodes as
        // Unknown, so the reader skips it rather than failing the stream.
        assert_eq!(
            serde_json::from_value::<Event>(json!({"type":"control","action":"x"})).unwrap(),
            Event::Unknown
        );
        assert_eq!(
            serde_json::from_value::<Event>(
                json!({"type":"heartbeat","observed_at_unix_ms":1,"metrics":{},"later":true})
            )
            .unwrap(),
            Event::Heartbeat {
                observed_at_unix_ms: 1,
                metrics: Metrics::default()
            }
        );
        // A 0.1/0.2 helper's stats omit `stores_incomplete`; it reads as zero.
        let older = json!({"type":"snapshot","sessions":[],"stats":{"dropped_lines":0,
            "unparsable_lines":0,"truncated_files":0,"blocks_without_session":0,
            "output_truncated":false,"enumeration_failed":false,"sessions_omitted":0}});
        assert_eq!(
            serde_json::from_value::<Event>(older).unwrap(),
            Event::Snapshot {
                sessions: vec![],
                stats: Stats::default()
            }
        );
    }

    #[test]
    fn oversize_snapshot_drops_oldest_sessions_and_counts_them() {
        let sessions: Vec<_> = (0..8).map(|id| record(id, "t".repeat(200_000))).collect();
        let line = snapshot_line(sessions, Stats::default()).unwrap();
        assert!(line.len() <= MAX_EVENT_LINE_BYTES);
        let Event::Snapshot { sessions, stats } = serde_json::from_slice(&line).unwrap() else {
            panic!("snapshot")
        };
        assert_eq!(sessions.len() + stats.sessions_omitted, 8);
        assert!(stats.sessions_omitted > 0);
        // The newest sessions are kept.
        assert_eq!(sessions[0].session_id, "s0");

        // Trimming keeps as many sessions as fit: one more would not.
        let kept = sessions.len();
        let one_more: Vec<_> = (0..=kept)
            .map(|id| record(id, "t".repeat(200_000)))
            .collect();
        let untrimmed = event_line(&Event::Snapshot {
            sessions: one_more,
            stats: Stats::default(),
        })
        .unwrap();
        assert!(untrimmed.len() > MAX_EVENT_LINE_BYTES);

        let small = snapshot_line(vec![record(0, "t".into())], Stats::default()).unwrap();
        let Event::Snapshot { stats, .. } = serde_json::from_slice(&small).unwrap() else {
            panic!("snapshot")
        };
        assert_eq!(stats.sessions_omitted, 0);
    }
}
