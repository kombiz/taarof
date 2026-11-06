use agent_session_core::remote_enumeration::{
    REMOTE_MAX_OUTPUT_BYTES, parse_remote_enumeration_output, remote_enumeration_script,
};
use agent_session_core::remote_stream::{Event, Host, Memory, Stats};
use std::{fs, io, time::Duration};

pub fn snapshot(host: &str, mut alive: impl FnMut() -> io::Result<()>) -> Event {
    let mut stats = Stats::default();
    let sessions = match crate::process::output(
        crate::process::command("sh").args(["-c", &remote_enumeration_script()]),
        Duration::from_secs(5),
        REMOTE_MAX_OUTPUT_BYTES,
        &mut alive,
    ) {
        Ok(output) => {
            stats.output_truncated = output.truncated;
            let (sessions, records) = parse_remote_enumeration_output(
                host,
                host,
                &String::from_utf8_lossy(&output.bytes),
            );
            stats.records = records;
            sessions
        }
        Err(_) => {
            stats.enumeration_failed = true;
            vec![]
        }
    };
    let load = fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);
    let mem = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| memory(&s));
    let nproc = crate::process::output(
        &mut crate::process::command("nproc"),
        Duration::from_secs(1),
        128,
        &mut alive,
    )
    .ok()
    .filter(|out| !out.truncated)
    .and_then(|out| {
        String::from_utf8(out.bytes)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    })
    .filter(|n| *n > 0);
    let tmux_session_count = crate::process::output(
        crate::process::command("tmux").args(["list-sessions", "-F", "1"]),
        Duration::from_secs(1),
        65536,
        &mut alive,
    )
    .ok()
    .filter(|out| !out.truncated)
    .and_then(|out| {
        let text = String::from_utf8(out.bytes).ok()?;
        if text.lines().all(|line| line == "1") {
            Some(text.lines().count() as u32)
        } else {
            None
        }
    });
    Event::Snapshot {
        host: Host {
            load,
            mem,
            nproc,
            tmux_session_count,
        },
        sessions,
        stats,
    }
}

fn memory(text: &str) -> Option<Memory> {
    let kib = |key: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? != key {
                return None;
            }
            let value: u64 = fields.next()?.parse().ok()?;
            (fields.next()? == "kB").then_some(value)?.checked_mul(1024)
        })
    };
    let total_bytes = kib("MemTotal:")?;
    let available = kib("MemAvailable:")?;
    if total_bytes == 0 {
        return None;
    }
    Some(Memory {
        total_bytes,
        used_bytes: total_bytes.checked_sub(available)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meminfo_matches_free_used_memory_and_rejects_bad_data() {
        assert_eq!(
            memory("MemTotal: 100 kB\nMemAvailable: 40 kB\n"),
            Some(Memory {
                total_bytes: 102400,
                used_bytes: 61440
            })
        );
        assert_eq!(memory("MemTotal: 100 kB\n"), None);
        assert_eq!(memory("MemTotal: 0 kB\nMemAvailable: 1 kB\n"), None);
        assert_eq!(memory("MemTotal: 1 kB\nMemAvailable: 2 kB\n"), None);
    }
}
