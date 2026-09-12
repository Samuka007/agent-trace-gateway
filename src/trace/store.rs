//! In-process turn record store (observable via the control endpoint).
//!
//! v0.3.9 bounded semantics: the local turn copies are a DEBUG WINDOW, not
//! authoritative storage — the authority is Langfuse after the OTLP export.
//! The store keeps at most `max_records` entries AND at most `max_bytes`
//! estimated payload bytes (raw_request + raw_response + input + output
//! lengths); over budget the OLDEST records are dropped. Every drop is
//! counted (`dropped()`) — loss is acceptable and transparently observable.

use atg_model::TurnRecord;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const DEFAULT_MAX_RECORDS: usize = 1000;
const DEFAULT_MAX_BYTES: usize = 256 << 20;

#[derive(Clone)]
pub struct TraceStore {
    records: Arc<Mutex<StoreInner>>,
}

struct StoreInner {
    deque: std::collections::VecDeque<TurnRecord>,
    estimated_bytes: usize,
    dropped: AtomicU64,
    max_records: usize,
    max_bytes: usize,
}

impl Default for TraceStore {
    fn default() -> Self {
        Self::with_caps(DEFAULT_MAX_RECORDS, DEFAULT_MAX_BYTES)
    }
}

impl TraceStore {
    /// Env-driven construction (follows the CaptureCap pattern).
    pub fn new() -> Self {
        let max_records = std::env::var("ATG_STORE_MAX_RECORDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_RECORDS);
        let max_bytes = std::env::var("ATG_STORE_MAX_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_BYTES);
        Self::with_caps(max_records, max_bytes)
    }

    pub fn with_caps(max_records: usize, max_bytes: usize) -> Self {
        Self {
            records: Arc::new(Mutex::new(StoreInner {
                deque: std::collections::VecDeque::new(),
                estimated_bytes: 0,
                dropped: AtomicU64::new(0),
                max_records: max_records.max(1),
                max_bytes: max_bytes.max(1),
            })),
        }
    }

    pub fn push(&self, record: TurnRecord) {
        let mut inner = self.records.lock();
        let size = estimate(&record);
        // Enforce both budgets: drop the OLDEST until the new record fits.
        while inner.deque.len() >= inner.max_records
            || inner.estimated_bytes.saturating_add(size) > inner.max_bytes
        {
            match inner.deque.pop_front() {
                Some(old) => {
                    inner.estimated_bytes = inner.estimated_bytes.saturating_sub(estimate(&old));
                    inner.dropped.fetch_add(1, Ordering::Relaxed);
                }
                None => break, // deque exhausted: the new record alone stays
            }
        }
        inner.estimated_bytes = inner.estimated_bytes.saturating_add(size);
        inner.deque.push_back(record);
    }

    /// Bounded snapshot: at most `limit` newest records, skipping `offset`
    /// of the newer ones (offset 0 = the newest end). `limit` 0 = unlimited
    /// (bounded anyway by the store caps). Order: oldest → newest, matching
    /// the pre-bounds endpoint shape.
    pub fn snapshot_bounded(&self, limit: usize, offset: usize) -> Vec<TurnRecord> {
        let inner = self.records.lock();
        let total = inner.deque.len();
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(if limit == 0 { total } else { limit });
        // end >= start by construction (both derive from the same
        // saturating chain) — take() is the lint-explicit spelling.
        let count = end.saturating_sub(start);
        inner
            .deque
            .iter()
            .skip(start)
            .take(count)
            .cloned()
            .collect()
    }

    /// Records dropped by the budgets since startup.
    pub fn dropped(&self) -> u64 {
        self.records.lock().dropped.load(Ordering::Relaxed)
    }
}

/// Payload-size estimate for the byte budget: the verbatim + extracted
/// content lengths (the fields that dominate a record's footprint).
#[allow(clippy::arithmetic_side_effects)] // pure summation of measured lens; no overflow at realistic sizes
fn estimate(record: &TurnRecord) -> usize {
    record.raw_request.len()
        + record.raw_response.len()
        + record.user_input.len()
        + record.final_output.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_with_payload(request_bytes: usize, marker: &str) -> TurnRecord {
        TurnRecord {
            raw_request: "x".repeat(request_bytes),
            user_input: marker.to_string(),
            ..Default::default()
        }
    }

    /// Count budget: pushing beyond max_records drops the OLDEST.
    #[test]
    fn push_over_record_cap_drops_oldest() {
        let store = TraceStore::with_caps(3, usize::MAX);
        for i in 0..5 {
            store.push(record_with_payload(10, &format!("r{i}")));
        }
        let snap = store.snapshot_bounded(0, 0);
        assert_eq!(snap.len(), 3, "bounded to the newest 3");
        assert_eq!(snap[0].user_input, "r2", "oldest two dropped");
        assert_eq!(snap[2].user_input, "r4");
        assert_eq!(store.dropped(), 2, "every drop is counted");
    }

    /// Byte budget: big payloads evict older records to stay under budget.
    #[test]
    fn push_over_byte_budget_drops_oldest() {
        // Budget 4 KiB; three ~1 KiB records (incl. the marker) fit — the
        // fourth forces one drop.
        let store = TraceStore::with_caps(usize::MAX, 4 * 1024);
        for i in 0..3 {
            store.push(record_with_payload(1024, &format!("b{i}")));
        }
        assert_eq!(store.snapshot_bounded(0, 0).len(), 3);
        store.push(record_with_payload(1024, "b3"));
        let snap = store.snapshot_bounded(0, 0);
        assert_eq!(snap.len(), 3, "still bounded");
        assert_eq!(snap[0].user_input, "b1", "b0 evicted first");
        assert!(store.dropped() >= 1);
    }

    /// Pagination: limit/offset window over the newest end.
    #[test]
    fn snapshot_bounded_paginates_the_newest_end() {
        let store = TraceStore::with_caps(10, usize::MAX);
        for i in 0..6 {
            store.push(record_with_payload(1, &format!("p{i}")));
        }
        let newest2 = store.snapshot_bounded(2, 0);
        assert_eq!(newest2.len(), 2);
        assert_eq!(newest2[1].user_input, "p5", "offset 0 ends at the newest");
        let skipped = store.snapshot_bounded(2, 2);
        assert_eq!(skipped[1].user_input, "p3", "offset skips newer records");
    }
}
