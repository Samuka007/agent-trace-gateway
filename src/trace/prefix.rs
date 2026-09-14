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
//! (env ATG_STITCH_TTL_MS, default 24h). Over/evicted requests degrade to
//! independent single-turn records; no errors are produced.
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
        Self {
            states: Mutex::new(HashMap::new()),
            capacity,
            ttl,
            next_nonce: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            lock_wait_ns: AtomicU64::new(0),
            lock_hold_ns: AtomicU64::new(0),
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
        // critical section.
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
        let key = (scope.to_string(), head);
        // TTL purge.
        let ttl = self.ttl;
        let before = states.len();
        states.retain(|_, st| now.duration_since(st.last_used) < ttl);
        self.expired.fetch_add(
            before.saturating_sub(states.len()) as u64,
            Ordering::Relaxed,
        );
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
