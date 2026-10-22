//! Capture size caps: bound verbatim content in turn records with a
//! deterministic truncation marker carrying original and captured byte counts.
//! Not a security control (D4: no redaction) — pure unbounded-growth defense.
// PANIC-AUDIT v0.3.8: audited file — serde_json Value key-index (miss →
// Null, never panics on objects) and provably-bounded slices/arithmetic on
// locally-owned buffers (wire bodies capped by the capture layer). The
// indexing/arithmetic lints are syntax-broad here; tracked in the
// PanicAudit issue.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The verbatim-capture cap (config `capture.max_bytes`). A hot key: the
/// current value rides an atomic that the reload path re-seeds, so the
/// next `bound()`/`max_bytes()` read observes the new cap without any
/// request-path snapshot plumbing.
#[derive(Clone)]
pub struct CaptureCap {
    max_bytes: Arc<AtomicUsize>,
}

impl Default for CaptureCap {
    fn default() -> Self {
        Self::new(16 << 20)
    }
}

impl CaptureCap {
    /// Explicit-value constructor (no env): the startup value comes from
    /// the config file.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes: Arc::new(AtomicUsize::new(max_bytes.max(1))),
        }
    }

    /// Re-seed the cap (reload path).
    pub fn set_max(&self, max_bytes: usize) {
        self.max_bytes.store(max_bytes.max(1), Ordering::Relaxed);
    }

    /// The currently configured cap in bytes — the drain capture growth
    /// bound (v0.3.6): after the client dies, raw response bytes stop being
    /// appended beyond this (the stream itself keeps being consumed).
    pub fn max_bytes(&self) -> usize {
        self.max_bytes.load(Ordering::Relaxed)
    }

    /// Bound captured content. Below the cap the content passes through
    /// verbatim; above it the verbatim prefix is kept up to the cap (on a
    /// UTF-8 boundary) followed by the deterministic marker.
    pub fn bound(&self, data: &[u8]) -> String {
        let max_bytes = self.max_bytes();
        if data.len() <= max_bytes {
            return String::from_utf8_lossy(data).to_string();
        }
        let mut end = max_bytes;
        // Back up to a UTF-8 character boundary (skip continuation bytes).
        while end > 0 && (data[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        let mut out = String::from_utf8_lossy(&data[..end]).to_string();
        out.push_str(&format!(
            "[truncated:original_bytes={},captured_bytes={}]",
            data.len(),
            end
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_respects_the_current_cap() {
        let cap = CaptureCap::new(8);
        assert_eq!(cap.bound(b"12345678"), "12345678");
        assert_eq!(
            cap.bound(b"1234567890"),
            "12345678[truncated:original_bytes=10,captured_bytes=8]"
        );
        // Hot cap: re-seed and the next bound observes the new value.
        cap.set_max(4);
        assert_eq!(cap.max_bytes(), 4);
        assert_eq!(
            cap.bound(b"1234567890"),
            "1234[truncated:original_bytes=10,captured_bytes=4]"
        );
    }
}
