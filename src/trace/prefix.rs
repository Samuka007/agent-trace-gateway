//! Prefix stitching state: fingerprint chains keyed by (credential scope,
//! head = system+first-user). State is O(turns) — one 32-byte fingerprint per
//! message, never the history bytes — so very large conversations do not
//! amplify memory.
//!
//! Rules (empirically calibrated against real omp traffic):
//! - chain ⊆ new messages (strict prefix)  -> extend, same synthetic session
//! - same head but chain diverges/shortens -> new segment + breakpoint mark
//! - new head                              -> new session, no mark (a different
//!   conversation cannot be told from a rewrite, so nothing is asserted)
//!
//! Bounds: LRU capacity (env ATG_STITCH_CAPACITY, default 100_000) + TTL
//! (env ATG_STITCH_TTL_MS, default 24h). The TTL expiry pass is AMORTIZED
//! (ATG#5): it runs at most once per SWEEP_INTERVAL_MS instead of on every
//! request, and expiry is enforced per entry on lookup — an expired chain is
//! never extended either way, so the observable contract is unchanged.
//! Over/evicted requests degrade to independent single-turn records; no
//! errors are produced.
// PANIC-AUDIT v0.3.8: audited file — serde_json Value key-index (miss →
// Null, never panics on objects) and provably-bounded slices/arithmetic on
// locally-owned buffers (wire bodies capped by the capture layer). The
// indexing/arithmetic lints are syntax-broad here; tracked in the
// PanicAudit issue.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const FINGERPRINT_KIND: &str = "pfx";
const DEFAULT_CAPACITY: usize = 100_000;
const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);

/// TTL sweep cadence (ATG#5). The expiry pass walks the whole table while
/// holding `states`; running it on EVERY request made the stitcher a
/// millisecond-scale process-wide serial section as soon as the table grew
/// (production tables hold up to 10^5 chains). It now runs at most once per
/// interval — TTL semantics are preserved by the per-entry expiry check on
/// lookup, and the memory bound by the capacity check at insert.
const SWEEP_INTERVAL_MS: u64 = 30_000;

pub struct PrefixStitcher {
    states: Mutex<HashMap<(String, String), ChainState>>,
    capacity: usize,
    ttl: Duration,
    next_nonce: AtomicU64,
    /// Chains dropped by TTL expiry / by the capacity LRU since startup
    /// (observability: the table size drives the O(table) sweep cost that
    /// the per-request path pays — ATG#5).
    expired: AtomicU64,
    evicted: AtomicU64,
    /// Cumulative time spent WAITING for `states` and HOLDING it, in
    /// nanoseconds (ATG#5 attribution: how much of the per-request cost
    /// under N workers is this serial section — wait is caused by hold).
    lock_wait_ns: AtomicU64,
    lock_hold_ns: AtomicU64,
    /// Monotonic clock anchor for the sweep cadence (a wall-clock jump must
    /// neither suppress nor hurry it).
    clock: Instant,
    /// Millis since `clock` at the last sweep. Relaxed: a redundant
    /// concurrent sweep is idempotent work and the cadence is approximate.
    last_sweep_ms: AtomicU64,
    /// Full-table sweeps performed since startup (ATG#5 amortization nail).
    sweeps: AtomicU64,
    /// Per-instance salt: keeps the synthetic id namespace scoped to this
    /// process so a restarted gateway never silently continues an old chain.
    salt: [u8; 8],
}

struct ChainState {
    /// Per-message fingerprints of the chain head, in order.
    chain: Vec<[u8; 32]>,
    /// Synthetic session id of the current segment.
    session_id: String,
    last_used: Instant,
}

impl Default for PrefixStitcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefixStitcher {
    pub fn new() -> Self {
        let capacity = std::env::var("ATG_STITCH_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CAPACITY);
        let ttl = std::env::var("ATG_STITCH_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TTL);
        Self::with_params(ttl, capacity)
    }

    /// Explicit-bounds constructor (env parsing lives in `new`); tests pin
    /// small TTL/capacity without touching process env.
    fn with_params(ttl: Duration, capacity: usize) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            capacity,
            ttl,
            next_nonce: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            lock_wait_ns: AtomicU64::new(0),
            lock_hold_ns: AtomicU64::new(0),
            clock: Instant::now(),
            last_sweep_ms: AtomicU64::new(0),
            sweeps: AtomicU64::new(0),
            salt: instance_salt(),
        }
    }

    /// Chains currently held. This is the number that scales the sweep cost
    /// (see ATG#5); scrape-time only — the lock is held for one `len()`.
    pub fn entries(&self) -> usize {
        self.states.lock().len()
    }

    /// Configured LRU capacity (ATG_STITCH_CAPACITY, default 100_000).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Chains dropped by TTL expiry since startup.
    pub fn expired_total(&self) -> u64 {
        self.expired.load(Ordering::Relaxed)
    }

    /// Chains dropped by the capacity LRU since startup.
    pub fn evicted_total(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    /// Cumulative nanoseconds blocked on the stitcher lock (contention).
    pub fn lock_wait_ns_total(&self) -> u64 {
        self.lock_wait_ns.load(Ordering::Relaxed)
    }

    /// Cumulative nanoseconds inside the stitcher lock (critical section).
    pub fn lock_hold_ns_total(&self) -> u64 {
        self.lock_hold_ns.load(Ordering::Relaxed)
    }

    /// Classify one credential-scoped request into a synthetic session.
    /// Returns (session_id, breakpoint).
    pub fn assign(&self, scope: &str, messages: &[serde_json::Value]) -> (String, bool) {
        // Fingerprint work (one SHA-256 per message) stays OUTSIDE the
        // critical section: it is pure computation over the request body.
        let head = head_key(messages);
        let fps: Vec<[u8; 32]> = messages.iter().map(message_fingerprint).collect();
        let now = Instant::now();
        let wait_start = Instant::now();
        let mut states = self.states.lock();
        self.lock_wait_ns
            .fetch_add(wait_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let hold_start = Instant::now();
        let out = self.assign_locked(scope, head, &mut states, fps, now);
        drop(states);
        self.lock_hold_ns
            .fetch_add(hold_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }

    /// The critical section proper (see `assign` for the metering wrapper).
    fn assign_locked(
        &self,
        scope: &str,
        head: String,
        states: &mut HashMap<(String, String), ChainState>,
        fps: Vec<[u8; 32]>,
        now: Instant,
    ) -> (String, bool) {
        let now_ms = now.saturating_duration_since(self.clock).as_millis() as u64;
        // Amortized TTL sweep (ATG#5): the table-wide pass is O(chains) and
        // used to run while holding this lock on every request — the
        // regression this issue names. It now runs at most once per interval.
        if now_ms.saturating_sub(self.last_sweep_ms.load(Ordering::Relaxed)) >= SWEEP_INTERVAL_MS {
            let ttl = self.ttl;
            let before = states.len();
            states.retain(|_, st| now.duration_since(st.last_used) < ttl);
            self.expired.fetch_add(
                before.saturating_sub(states.len()) as u64,
                Ordering::Relaxed,
            );
            self.last_sweep_ms.store(now_ms, Ordering::Relaxed);
            self.sweeps.fetch_add(1, Ordering::Relaxed);
        }
        let key = (scope.to_string(), head);
        // TTL semantics without a per-request purge: an expired chain is dead
        // — it must neither be extended nor have its old session id reopened
        // (the sweep above may not run for a whole interval).
        if states
            .get(&key)
            .is_some_and(|st| now.duration_since(st.last_used) >= self.ttl)
        {
            let _ = states.remove(&key);
        }
        if let Some(entry) = states.get_mut(&key) {
            entry.last_used = now;
            if fps.len() >= entry.chain.len() && fps[..entry.chain.len()] == entry.chain[..] {
                // Strict prefix extension.
                entry.chain = fps;
                return (entry.session_id.clone(), false);
            }
            // Same head, divergent or shortened history: compaction breakpoint.
            let new_session = self.new_session_id(&fps_concat(&fps));
            entry.chain = fps;
            entry.session_id = new_session.clone();
            return (new_session, true);
        }
        // New chain: enforce LRU capacity before inserting.
        if states.len() >= self.capacity {
            let lru_key = states
                .iter()
                .min_by_key(|(_, st)| st.last_used)
                .map(|(k, _)| k.clone());
            if let Some(k) = lru_key {
                states.remove(&k);
                self.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
        let session_id = self.new_session_id(&fps_concat(&fps));
        states.insert(
            key,
            ChainState {
                chain: fps,
                session_id: session_id.clone(),
                last_used: now,
            },
        );
        (session_id, false)
    }

    /// Session ids carry a nonce so a reopened chain (after eviction/TTL)
    /// never silently merges with its former trajectory.
    fn new_session_id(&self, seed: &[u8]) -> String {
        let nonce = self.next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut h = Sha256::new();
        h.update(FINGERPRINT_KIND.as_bytes());
        h.update(self.salt);
        h.update(seed);
        h.update(nonce.to_be_bytes());
        let hexed = hex::encode(h.finalize());
        format!("{FINGERPRINT_KIND}:{}", &hexed[..32])
    }
}

/// Process-scoped salt (not cryptographic): pid, clock and a static address
/// together vary per process instance.
fn instance_salt() -> [u8; 8] {
    static ANCHOR: u8 = 0;
    let mut h = Sha256::new();
    h.update(std::process::id().to_be_bytes());
    h.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_be_bytes())
            .unwrap_or_default(),
    );
    h.update((&ANCHOR as *const u8 as usize).to_be_bytes());
    // PANIC-AUDIT v0.3.8: sha256 finalize yields exactly 32 bytes; [..8]
    // is a provable fixed-length slice and try_into on 8 bytes is total
    // (class iii, len contract).
    #[allow(clippy::indexing_slicing, clippy::unwrap_used)]
    h.finalize()[..8].try_into().unwrap()
}

/// Head key: fingerprints of the first two messages (system + first user).
fn head_key(messages: &[serde_json::Value]) -> String {
    let mut h = Sha256::new();
    for m in messages.iter().take(2) {
        h.update(message_fingerprint(m));
    }
    hex::encode(h.finalize())
}

fn message_fingerprint(m: &serde_json::Value) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(m.to_string().as_bytes());
    h.finalize().into()
}

fn fps_concat(fps: &[[u8; 32]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fps.len() * 32);
    for f in fps {
        out.extend_from_slice(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(head: &str) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": head}),
        ]
    }

    /// Age the cadence clock so the next `assign` is due for a sweep — the
    /// 30 s interval is not something a test waits out. (White-box on
    /// purpose: the cadence is the invariant under test.)
    fn make_sweep_due(stitcher: &mut PrefixStitcher) {
        stitcher.clock = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("clock underflow");
    }

    /// ATG#5 amortization invariant: the table-wide TTL pass is NOT on the
    /// per-request path — a burst of assigns inside one interval performs no
    /// sweep at all, and exactly one once the interval is due. A regression to
    /// the old per-request `retain` fails this on the first assertion.
    #[test]
    fn ttl_sweep_is_amortized_not_per_request() {
        let mut stitcher = PrefixStitcher::with_params(Duration::from_secs(3600), 1024);
        for i in 0..64 {
            stitcher.assign("scope", &conversation(&format!("head-{i}")));
        }
        assert_eq!(
            stitcher.sweeps.load(Ordering::Relaxed),
            0,
            "the O(table) sweep must not run per request"
        );
        assert_eq!(stitcher.states.lock().len(), 64, "every head was inserted");
        make_sweep_due(&mut stitcher);
        stitcher.assign("scope", &conversation("head-again"));
        assert_eq!(
            stitcher.sweeps.load(Ordering::Relaxed),
            1,
            "exactly one sweep once due"
        );
    }

    /// A live chain keeps extending between sweeps (no behavioural drift).
    #[test]
    fn live_chain_extends_within_one_interval() {
        let stitcher = PrefixStitcher::with_params(Duration::from_secs(3600), 1024);
        let (first, bp_first) = stitcher.assign("scope", &conversation("head-x"));
        let (second, bp_second) = stitcher.assign("scope", &conversation("head-x"));
        assert_eq!(first, second, "a live chain must keep its session");
        assert!(!bp_first && !bp_second, "extension is not a breakpoint");
        assert_eq!(stitcher.sweeps.load(Ordering::Relaxed), 0);
    }

    /// TTL semantics survive the amortized sweep: an expired chain is neither
    /// extended nor reopened under its old session id — the per-entry expiry
    /// check on lookup stands in for the per-request purge.
    #[test]
    fn expired_chain_is_not_extended_without_a_sweep() {
        let stitcher = PrefixStitcher::with_params(Duration::from_millis(20), 1024);
        let (first, bp_first) = stitcher.assign("scope", &conversation("head-1"));
        assert!(!bp_first);
        std::thread::sleep(Duration::from_millis(40));
        let (second, bp_second) = stitcher.assign("scope", &conversation("head-1"));
        assert_ne!(second, first, "an expired chain must not be extended");
        assert!(
            !bp_second,
            "an expired chain reopens as a new chain, not a breakpoint"
        );
    }

    /// The sweep, when due, reclaims expired entries (the memory-side half of
    /// what the per-request purge used to do).
    #[test]
    fn due_sweep_reclaims_expired_entries() {
        let mut stitcher = PrefixStitcher::with_params(Duration::from_millis(20), 1024);
        stitcher.assign("scope", &conversation("head-a"));
        stitcher.assign("scope", &conversation("head-b"));
        std::thread::sleep(Duration::from_millis(40));
        make_sweep_due(&mut stitcher);
        stitcher.assign("scope", &conversation("head-c"));
        assert_eq!(
            stitcher.states.lock().len(),
            1,
            "both expired chains must be reclaimed by the due sweep"
        );
        assert_eq!(stitcher.sweeps.load(Ordering::Relaxed), 1);
    }

    /// Capacity enforcement is unchanged by the amortized sweep: the table
    /// never exceeds `capacity`, and the evicted chain reopens fresh.
    #[test]
    fn capacity_bound_holds_without_the_sweep() {
        let stitcher = PrefixStitcher::with_params(Duration::from_secs(3600), 2);
        let (a1, _) = stitcher.assign("scope", &conversation("a"));
        let (b1, _) = stitcher.assign("scope", &conversation("b"));
        let (c1, _) = stitcher.assign("scope", &conversation("c"));
        let (a2, _) = stitcher.assign("scope", &conversation("a"));
        assert_ne!(a2, a1, "an evicted chain must reopen, not merge");
        assert_ne!(a2, b1);
        assert_ne!(a2, c1);
        assert!(
            stitcher.states.lock().len() <= 2,
            "the LRU capacity bound must hold"
        );
    }
}
