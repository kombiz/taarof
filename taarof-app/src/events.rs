use serde::Serialize;
use std::cell::Cell;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_EVENT_LIMIT: usize = 100;
pub const MAX_EVENT_LIMIT: usize = 500;
const DEFAULT_EVENT_CAPACITY: usize = 512;
pub const DROP_RATE_WINDOW_MS: u64 = 60_000;
pub const CURSOR_GAP_ACTIVE_WINDOW_MS: u64 = 120_000;
/// Width of a single drop-rate bucket. Drops are aggregated into fixed-width
/// time buckets so `drop_window` is bounded to roughly one bucket per second of
/// the rate window regardless of how many drops a burst produces.
const DROP_RATE_BUCKET_MS: u64 = 1_000;
/// Hard bound on retained drop buckets: at most one per bucket across the rate
/// window, plus slack for the in-progress bucket and boundary rounding.
const DROP_WINDOW_MAX_BUCKETS: usize = (DROP_RATE_WINDOW_MS / DROP_RATE_BUCKET_MS + 2) as usize;

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    pub seq: u64,
    pub ts_unix_ms: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct EventQuery {
    pub events: Vec<EventRecord>,
    pub next_seq: u64,
    pub high_watermark: u64,
    pub oldest_seq: Option<u64>,
    pub limit: usize,
    pub gap: bool,
    pub gap_from: Option<u64>,
    pub gap_to: Option<u64>,
    pub resnapshot_required: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRetentionHealth {
    pub state: String,
    pub reason: Option<String>,
    pub dropped_total: u64,
    pub last_dropped_at_unix_ms: Option<u64>,
    pub drops_in_window: u64,
    pub rate_window_ms: u64,
    pub cursor_gaps_total: u64,
    pub last_cursor_gap_at_unix_ms: Option<u64>,
    pub active_cursor_loss: bool,
    pub abnormal_rate: bool,
}

#[derive(Debug)]
pub struct EventStore {
    next_seq: u64,
    entries: VecDeque<EventRecord>,
    capacity: usize,
    dropped: u64,
    last_dropped_at_unix_ms: Option<u64>,
    /// Time-bucketed drop counts: `(bucket_start_ms, count, last_drop_ms)`, oldest first.
    /// Bounded to `DROP_WINDOW_MAX_BUCKETS` entries so burst size never grows
    /// memory or `retention_health` iteration cost. The exact last-drop time
    /// keeps drops near a bucket boundary in the rate window for the full
    /// configured duration.
    drop_window: VecDeque<(u64, u64, u64)>,
    // Cursor-gap accounting is observed on the read path (`query`), so it uses
    // interior mutability to keep `query` a shared-borrow operation.
    cursor_gaps: Cell<u64>,
    last_cursor_gap_at_unix_ms: Cell<Option<u64>>,
    live_sink: Option<tokio::sync::broadcast::Sender<serde_json::Value>>,
    history_sink: Option<crate::history::HistoryHandle>,
}

impl Default for EventStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_EVENT_CAPACITY)
    }
}

impl EventStore {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            next_seq: 1,
            entries: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            dropped: 0,
            last_dropped_at_unix_ms: None,
            drop_window: VecDeque::with_capacity(DROP_WINDOW_MAX_BUCKETS),
            cursor_gaps: Cell::new(0),
            last_cursor_gap_at_unix_ms: Cell::new(None),
            live_sink: None,
            history_sink: None,
        }
    }

    pub fn emit(&mut self, event_type: impl Into<String>, payload: serde_json::Value) -> u64 {
        self.emit_with_timestamp(unix_time_ms(), event_type, payload)
    }

    fn emit_with_timestamp(
        &mut self,
        ts_unix_ms: u64,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;

        if self.entries.len() >= self.capacity {
            let dropped = self.entries.pop_front();
            self.dropped += 1;
            // System wall time can move backwards. Keep retention bookkeeping
            // monotonic so the latest observed drop never makes the health
            // timestamp regress or creates out-of-order rate buckets.
            let drop_observed_at = self
                .last_dropped_at_unix_ms
                .map_or(ts_unix_ms, |last_drop| last_drop.max(ts_unix_ms));
            self.last_dropped_at_unix_ms = Some(drop_observed_at);
            while self.drop_window.front().is_some_and(|(_, _, last_drop)| {
                drop_observed_at.saturating_sub(*last_drop) > DROP_RATE_WINDOW_MS
            }) {
                self.drop_window.pop_front();
            }
            let drops_before: u64 = self.drop_window.iter().map(|(_, count, _)| *count).sum();
            let episode_start = drops_before == 0;
            let bucket = drop_observed_at - (drop_observed_at % DROP_RATE_BUCKET_MS);
            match self.drop_window.back_mut() {
                Some((existing, count, last_drop)) if *existing == bucket => {
                    *count += 1;
                    *last_drop = drop_observed_at;
                }
                _ => self.drop_window.push_back((bucket, 1, drop_observed_at)),
            }
            while self.drop_window.len() > DROP_WINDOW_MAX_BUCKETS {
                self.drop_window.pop_front();
            }
            let drops_after = drops_before + 1;
            let capacity = self.capacity as u64;
            let abnormal_rate_crossed = drops_before < capacity && drops_after >= capacity;
            if episode_start || abnormal_rate_crossed {
                crate::diagnostics::record_event_drop(
                    format!(
                        "event retention overflowed; dropped {} records total from the in-memory ring",
                        self.dropped,
                    ),
                    Some(serde_json::json!({
                        "dropped_total": self.dropped,
                        "drops_in_window": drops_after,
                        "rate_window_ms": DROP_RATE_WINDOW_MS,
                        "abnormal_rate": drops_after >= capacity,
                        "capacity": self.capacity,
                        "dropped_seq": dropped.as_ref().map(|event| event.seq),
                        "oldest_retained_seq": self.entries.front().map(|event| event.seq),
                    })),
                );
            }
        }

        let event = EventRecord {
            seq,
            ts_unix_ms,
            event_type: event_type.into(),
            payload,
        };

        if let Some(tx) = &self.live_sink {
            let _ = tx.send(event.as_json());
        }

        if let Some(history) = &self.history_sink {
            history.try_record_event(&event);
        }

        self.entries.push_back(event);

        seq
    }

    #[cfg(test)]
    pub fn emit_at(
        &mut self,
        ts_unix_ms: u64,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> u64 {
        self.emit_with_timestamp(ts_unix_ms, event_type, payload)
    }

    pub fn install_live_sink(&mut self, tx: tokio::sync::broadcast::Sender<serde_json::Value>) {
        self.live_sink = Some(tx);
    }

    pub fn clear_live_sink(&mut self) {
        self.live_sink = None;
    }

    pub fn install_history_sink(&mut self, history: crate::history::HistoryHandle) {
        self.history_sink = Some(history);
    }

    pub fn clear_history_sink(&mut self) {
        self.history_sink = None;
    }

    pub fn query(&self, since_seq: Option<u64>, limit: Option<usize>) -> EventQuery {
        self.query_at(since_seq, limit, unix_time_ms())
    }

    fn query_at(&self, since_seq: Option<u64>, limit: Option<usize>, now: u64) -> EventQuery {
        let limit = limit
            .unwrap_or(DEFAULT_EVENT_LIMIT)
            .clamp(1, MAX_EVENT_LIMIT);
        let oldest_seq = self.oldest_seq();
        let gap = since_seq
            .zip(oldest_seq)
            .is_some_and(|(cursor, oldest)| cursor.saturating_add(1) < oldest);
        let (gap_from, gap_to) = if gap {
            let cursor = since_seq.expect("gap requires a caller cursor");
            (
                cursor.checked_add(1),
                oldest_seq.and_then(|oldest| oldest.checked_sub(1)),
            )
        } else {
            (None, None)
        };
        if gap {
            // Cursor-loss health is global, so all gap observations inside one
            // active window form a single episode. Refreshing the observation
            // timestamp keeps a consumer that is still reporting loss visible,
            // while the cumulative counter advances only after a quiet window.
            let last_gap_at = self.last_cursor_gap_at_unix_ms.get();
            if last_gap_at
                .is_none_or(|timestamp| now.saturating_sub(timestamp) > CURSOR_GAP_ACTIVE_WINDOW_MS)
            {
                self.cursor_gaps
                    .set(self.cursor_gaps.get().saturating_add(1));
            }
            self.last_cursor_gap_at_unix_ms.set(Some(
                last_gap_at.map_or(now, |timestamp| timestamp.max(now)),
            ));
        }
        let cursor = since_seq.unwrap_or(0);
        let events: Vec<EventRecord> = self
            .entries
            .iter()
            .filter(|event| event.seq > cursor)
            .take(limit)
            .cloned()
            .collect();
        let next_seq = events.last().map(|event| event.seq).unwrap_or(cursor);

        EventQuery {
            events,
            next_seq,
            high_watermark: self.high_watermark(),
            oldest_seq,
            limit,
            gap,
            gap_from,
            gap_to,
            resnapshot_required: gap,
        }
    }

    pub fn retention_health(&self) -> EventRetentionHealth {
        self.retention_health_at(unix_time_ms())
    }

    fn retention_health_at(&self, now: u64) -> EventRetentionHealth {
        let drops_in_window: u64 = self
            .drop_window
            .iter()
            .filter(|(_, _, last_drop)| now.saturating_sub(*last_drop) <= DROP_RATE_WINDOW_MS)
            .map(|(_, count, _)| *count)
            .sum();
        let last_cursor_gap_at = self.last_cursor_gap_at_unix_ms.get();
        let active_cursor_loss = last_cursor_gap_at
            .is_some_and(|timestamp| now.saturating_sub(timestamp) <= CURSOR_GAP_ACTIVE_WINDOW_MS);
        let abnormal_rate = drops_in_window >= self.capacity as u64;
        let reason = if active_cursor_loss {
            Some("consumer cursor fell behind retained history".to_string())
        } else if abnormal_rate {
            let rotations = drops_in_window / self.capacity as u64;
            Some(format!(
                "sustained event rate rotated the ring {rotations} times in 60s"
            ))
        } else {
            None
        };

        EventRetentionHealth {
            state: if active_cursor_loss || abnormal_rate {
                "degraded"
            } else {
                "ok"
            }
            .to_string(),
            reason,
            dropped_total: self.dropped,
            last_dropped_at_unix_ms: self.last_dropped_at_unix_ms,
            drops_in_window,
            rate_window_ms: DROP_RATE_WINDOW_MS,
            cursor_gaps_total: self.cursor_gaps.get(),
            last_cursor_gap_at_unix_ms: last_cursor_gap_at,
            active_cursor_loss,
            abnormal_rate,
        }
    }

    pub fn entries(&self) -> Vec<EventRecord> {
        self.entries.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn last_dropped_at_unix_ms(&self) -> Option<u64> {
        self.last_dropped_at_unix_ms
    }

    pub fn high_watermark(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub fn oldest_seq(&self) -> Option<u64> {
        self.entries.front().map(|e| e.seq)
    }
}

pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl EventRecord {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "seq": self.seq,
            "ts_unix_ms": self.ts_unix_ms,
            "event_type": self.event_type,
            "payload": self.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventStore, CURSOR_GAP_ACTIVE_WINDOW_MS, DROP_RATE_BUCKET_MS, DROP_RATE_WINDOW_MS,
        DROP_WINDOW_MAX_BUCKETS, MAX_EVENT_LIMIT,
    };
    use serde_json::json;

    #[test]
    fn event_store_assigns_monotonic_sequences() {
        let mut store = EventStore::with_capacity(4);

        assert_eq!(store.emit("one", json!({"n": 1})), 1);
        assert_eq!(store.emit("two", json!({"n": 2})), 2);
        assert_eq!(store.next_seq(), 3);
    }

    #[test]
    fn event_store_respects_since_seq_and_limit() {
        let mut store = EventStore::with_capacity(8);
        store.emit("one", json!({"n": 1}));
        store.emit("two", json!({"n": 2}));
        store.emit("three", json!({"n": 3}));

        let query = store.query(Some(1), Some(1));
        assert_eq!(query.limit, 1);
        assert_eq!(query.events.len(), 1);
        assert_eq!(query.events[0].seq, 2);
        assert_eq!(query.events[0].event_type, "two");
        assert_eq!(query.next_seq, 2);
        assert_eq!(query.high_watermark, 3);
    }

    #[test]
    fn event_store_clamps_large_limits() {
        let store = EventStore::with_capacity(8);
        let query = store.query(None, Some(MAX_EVENT_LIMIT + 50));
        assert_eq!(query.limit, MAX_EVENT_LIMIT);
    }

    #[test]
    fn event_store_keeps_cursor_stable_when_page_is_empty() {
        let mut store = EventStore::with_capacity(8);
        store.emit("one", json!({"n": 1}));
        store.emit("two", json!({"n": 2}));

        let query = store.query(Some(2), Some(10));
        assert!(query.events.is_empty());
        assert_eq!(query.next_seq, 2);
        assert_eq!(query.high_watermark, 2);
    }

    #[test]
    fn event_store_drops_oldest_entries_when_capacity_is_hit() {
        let mut store = EventStore::with_capacity(2);
        store.emit_at(10, "one", json!({"n": 1}));
        store.emit_at(20, "two", json!({"n": 2}));
        store.emit_at(30, "three", json!({"n": 3}));

        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 2);
        assert_eq!(entries[0].event_type, "two");
        assert_eq!(entries[1].seq, 3);
        assert_eq!(entries[1].event_type, "three");
        assert_eq!(store.capacity(), 2);
        assert_eq!(store.high_watermark(), 3);
        assert_eq!(store.oldest_seq(), Some(2));
        assert_eq!(store.dropped(), 1);
        assert_eq!(store.last_dropped_at_unix_ms(), Some(30));
    }

    #[test]
    fn completed_eviction_does_not_leave_retention_health_degraded() {
        let mut store = EventStore::with_capacity(2);
        store.emit("one", json!({}));
        store.emit("two", json!({}));
        store.emit("three", json!({}));

        let health = store.retention_health();
        assert_eq!(health.state, "ok");
        assert_eq!(health.dropped_total, 1);
        assert!(health.last_dropped_at_unix_ms.is_some());
        assert_eq!(health.drops_in_window, 1);
        assert!(!health.active_cursor_loss);
        assert!(!health.abnormal_rate);
    }

    #[test]
    fn stale_cursor_reports_exact_gap_and_active_cursor_loss() {
        let mut store = EventStore::with_capacity(2);
        store.emit("one", json!({}));
        store.emit("two", json!({}));
        store.emit("three", json!({}));

        let next_seq = store.next_seq();
        let query = store.query(Some(0), None);
        assert!(query.gap);
        assert_eq!(query.gap_from, Some(1));
        assert_eq!(query.gap_to, Some(1));
        assert!(query.resnapshot_required);
        assert_eq!(store.next_seq(), next_seq, "gap observation emits no event");

        let health = store.retention_health();
        assert_eq!(health.state, "degraded");
        assert_eq!(health.cursor_gaps_total, 1);
        assert!(health.active_cursor_loss);
        assert_eq!(
            health.reason.as_deref(),
            Some("consumer cursor fell behind retained history")
        );
    }

    #[test]
    fn cursor_loss_recovers_while_its_cumulative_counter_remains() {
        let mut store = EventStore::with_capacity(2);
        store.emit_at(10, "one", json!({}));
        store.emit_at(20, "two", json!({}));
        store.emit_at(30, "three", json!({}));
        let query = store.query_at(Some(0), None, 40);
        assert!(query.gap);
        let gap_at = 40;

        let recovered_at = gap_at + CURSOR_GAP_ACTIVE_WINDOW_MS + 1;
        let health = store.retention_health_at(recovered_at);
        assert_eq!(health.state, "ok");
        assert_eq!(health.cursor_gaps_total, 1);
        assert!(!health.active_cursor_loss);
    }

    #[test]
    fn sustained_rotation_degrades_then_recovers_after_rate_window() {
        let mut store = EventStore::with_capacity(3);
        for seq in 0..6 {
            store.emit_at(1_000 + seq, "event", json!({"seq": seq}));
        }

        let degraded = store.retention_health_at(1_005);
        assert_eq!(degraded.drops_in_window, 3);
        assert!(degraded.abnormal_rate);
        assert_eq!(degraded.state, "degraded");
        assert!(degraded
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("rotated the ring 1 times in 60s")));

        let recovered_at = 1_005 + DROP_RATE_WINDOW_MS + 1;
        store.emit_at(recovered_at, "later", json!({}));
        let recovered = store.retention_health_at(recovered_at);
        assert_eq!(recovered.drops_in_window, 1);
        assert!(!recovered.abnormal_rate);
        assert_eq!(recovered.state, "ok");
        assert_eq!(recovered.dropped_total, 4);
    }

    #[test]
    fn repeated_polling_at_same_stale_cursor_records_one_episode() {
        let mut store = EventStore::with_capacity(2);
        store.emit_at(10, "one", json!({}));
        store.emit_at(20, "two", json!({}));
        store.emit_at(30, "three", json!({}));

        assert!(store.query_at(Some(0), None, 40).gap);

        // The same consumer keeps polling the same stale cursor without
        // re-snapshotting. This must not inflate the episode counter, but it is
        // still active loss and therefore refreshes the observation timestamp.
        for now in 41..=45 {
            assert!(store.query_at(Some(0), None, now).gap);
        }
        let health = store.retention_health_at(45);
        assert_eq!(health.cursor_gaps_total, 1);
        assert_eq!(health.last_cursor_gap_at_unix_ms, Some(45));
        assert!(health.active_cursor_loss);

        // Health recovers only after no consumer has reported a gap for the
        // active window. A later observation starts a new cumulative episode.
        let recovered_at = 45 + CURSOR_GAP_ACTIVE_WINDOW_MS + 1;
        let health = store.retention_health_at(recovered_at);
        assert_eq!(health.state, "ok");
        assert!(!health.active_cursor_loss);
        assert_eq!(health.cursor_gaps_total, 1);

        assert!(store.query_at(Some(0), None, recovered_at).gap);
        let active_again = store.retention_health_at(recovered_at);
        assert_eq!(active_again.state, "degraded");
        assert!(active_again.active_cursor_loss);
        assert_eq!(active_again.cursor_gaps_total, 2);
    }

    #[test]
    fn cursor_gap_observation_time_does_not_regress_with_wall_clock() {
        let mut store = EventStore::with_capacity(2);
        store.emit_at(10, "one", json!({}));
        store.emit_at(20, "two", json!({}));
        store.emit_at(30, "three", json!({}));

        assert!(store.query_at(Some(0), None, 1_000).gap);
        assert!(store.query_at(Some(0), None, 900).gap);

        let health = store.retention_health_at(1_000 + CURSOR_GAP_ACTIVE_WINDOW_MS);
        assert_eq!(health.cursor_gaps_total, 1);
        assert_eq!(health.last_cursor_gap_at_unix_ms, Some(1_000));
        assert!(health.active_cursor_loss);
    }

    #[test]
    fn late_bucket_drop_remains_in_rate_window_for_full_duration() {
        let mut store = EventStore::with_capacity(2);
        store.emit_at(10, "one", json!({}));
        store.emit_at(20, "two", json!({}));
        store.emit_at(1_999, "three", json!({}));
        store.emit_at(2_000, "four", json!({}));

        let at_exact_boundary = store.retention_health_at(1_999 + DROP_RATE_WINDOW_MS);
        assert_eq!(at_exact_boundary.drops_in_window, 2);
        assert!(at_exact_boundary.abnormal_rate);

        let after_boundary = store.retention_health_at(1_999 + DROP_RATE_WINDOW_MS + 1);
        assert_eq!(after_boundary.drops_in_window, 1);
        assert!(!after_boundary.abnormal_rate);
    }

    #[test]
    fn drop_window_stays_bounded_under_sustained_burst() {
        let mut store = EventStore::with_capacity(4);
        let base = 1_000;
        for offset in 0..10_000u64 {
            store.emit_at(base + offset, "event", json!({}));
        }

        // Drops are bucketed by time, so the window can never exceed the
        // per-window bucket bound regardless of how many drops the burst
        // produced.
        assert!(
            store.drop_window.len() <= DROP_WINDOW_MAX_BUCKETS,
            "drop_window grew to {} buckets (bound {DROP_WINDOW_MAX_BUCKETS})",
            store.drop_window.len()
        );

        // All emits fall inside a single 60s rate window, so the counts stay
        // exact even though timestamps were aggregated.
        let health = store.retention_health_at(base + 9_999);
        assert_eq!(health.dropped_total, 10_000 - 4);
        assert_eq!(health.drops_in_window, 10_000 - 4);
    }

    #[test]
    fn drop_window_stays_bounded_when_wall_clock_moves_backwards() {
        let mut store = EventStore::with_capacity(1);
        let base = 200_000;
        for offset in 0..100u64 {
            store.emit_at(base - offset * DROP_RATE_BUCKET_MS, "event", json!({}));
        }

        // All drops after the initial retained event are one continuous burst.
        // A regressing wall clock must not regress the public last-drop time or
        // fragment that burst into artificial buckets that lose counts at the
        // hard bucket bound.
        assert_eq!(
            store.last_dropped_at_unix_ms(),
            Some(base - DROP_RATE_BUCKET_MS)
        );
        assert_eq!(store.drop_window.len(), 1);
        let health = store.retention_health_at(base - DROP_RATE_BUCKET_MS);
        assert_eq!(health.dropped_total, 99);
        assert_eq!(health.drops_in_window, 99);
        assert!(health.abnormal_rate);
    }

    #[test]
    fn contiguous_cursor_does_not_report_or_count_a_gap() {
        let mut store = EventStore::with_capacity(2);
        store.emit("one", json!({}));
        store.emit("two", json!({}));
        store.emit("three", json!({}));

        let query = store.query(Some(1), None);
        assert!(!query.gap);
        assert_eq!(query.gap_from, None);
        assert_eq!(query.gap_to, None);
        assert!(!query.resnapshot_required);
        assert_eq!(store.retention_health().cursor_gaps_total, 0);
    }
}
