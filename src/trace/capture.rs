//! Capture size caps: bound verbatim content in turn records with a
//! deterministic truncation marker carrying original and captured byte counts.
//! Not a security control (D4: no redaction) — pure unbounded-growth defense.
// PANIC-AUDIT v0.3.8: audited file — serde_json Value key-index (miss →
// Null, never panics on objects) and provably-bounded slices/arithmetic on
// locally-owned buffers (wire bodies capped by the capture layer). The
// indexing/arithmetic lints are syntax-broad here; tracked in the
// PanicAudit issue.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
pub struct CaptureCap {
    max_bytes: usize,
}

const DEFAULT_MAX_BYTES: usize = 16 << 20;

impl Default for CaptureCap {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureCap {
    pub fn new() -> Self {
        let max_bytes = std::env::var("ATG_CAPTURE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n: &usize| *n > 0)
            .unwrap_or(DEFAULT_MAX_BYTES);
        Self { max_bytes }
    }

    /// The configured cap in bytes — the drain capture growth bound
    /// (v0.3.6): after the client dies, raw response bytes stop being
    /// appended beyond this (the stream itself keeps being consumed).
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Bound captured content. Below the cap the content passes through
    /// verbatim; above it the verbatim prefix is kept up to the cap (on a
    /// UTF-8 boundary) followed by the deterministic marker.
    pub fn bound(&self, data: &[u8]) -> String {
        if data.len() <= self.max_bytes {
            return String::from_utf8_lossy(data).to_string();
        }
        let mut end = self.max_bytes;
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
