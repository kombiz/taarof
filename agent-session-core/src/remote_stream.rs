//! Version 1 of the observe-only NDJSON protocol. Each line is one event.
use crate::{remote_enumeration::RemoteParseStats, AgentSessionRecord};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello {
        protocol_version: u32,
        helper_version: String,
        host: String,
    },
    /// A complete replacement; slice 1 deliberately uses snapshots, not deltas.
    Snapshot {
        host: Host,
        sessions: Vec<AgentSessionRecord>,
        stats: Stats,
    },
    Heartbeat {
        observed_at_unix_ms: u64,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Host {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            host: Host {
                load: Some(2.0),
                mem: Some(Memory {
                    total_bytes: 100,
                    used_bytes: 50,
                }),
                nproc: Some(4),
                tmux_session_count: Some(0),
            },
            sessions: vec![],
            stats: Stats::default(),
        };
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "type":"snapshot", "host":{"load":2.0,"mem":{"total_bytes":100,"used_bytes":50},
                "nproc":4,"tmux_session_count":0}, "sessions":[],
                "stats":{"dropped_lines":0,"unparsable_lines":0,"truncated_files":0,
                    "blocks_without_session":0,"output_truncated":false,"enumeration_failed":false}
            })
        );
        let heartbeat = Event::Heartbeat {
            observed_at_unix_ms: 42,
        };
        assert_eq!(
            serde_json::to_value(&heartbeat).unwrap(),
            json!({"type":"heartbeat", "observed_at_unix_ms":42})
        );
        for event in [hello, snapshot, heartbeat] {
            let line = serde_json::to_string(&event).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
        }
        assert!(serde_json::from_value::<Event>(json!({"type":"control"})).is_err());
    }
}
