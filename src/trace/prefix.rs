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
            salt: instance_salt(),
        }
    }

    /// Classify one credential-scoped request into a synthetic session.
    /// Returns (session_id, breakpoint).
    pub fn assign(&self, scope: &str, messages: &[serde_json::Value]) -> (String, bool) {
        let head = head_key(messages);
        let fps: Vec<[u8; 32]> = messages.iter().map(message_fingerprint).collect();
        let mut states = self.states.lock();
        let now = Instant::now();
        // TTL purge.
        let ttl = self.ttl;
        states.retain(|_, st| now.duration_since(st.last_used) < ttl);
        let key = (scope.to_string(), head);
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
