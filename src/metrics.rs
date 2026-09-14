//! Prometheus-text observability for the `/__atg/metrics` endpoint
//! (v0.3.12, ATG issue #2).
//!
//! Zero dependencies: a fixed-bucket latency histogram over relaxed atomics
//! plus the text renderer. The hot path (`observe`) is one bounds scan
//! (<= 20 relaxed loads plus at most one relaxed increment per counter) —
//! no allocation, no locks, no blocking, no syscalls.
//!
//! Panic discipline (AGENTS.md): this code runs per request (`observe` at
//! request end, `render` on the metrics endpoint). No indexing, no
//! unwrap/expect, no fallible arithmetic — bucket slots resolve through
//! `position`/`get`, accumulation is `saturating`, formatting is
//! infallible.

use std::sync::atomic::{AtomicU64, Ordering};

/// Number of finite latency buckets (the `+Inf` bucket is the total count).
pub const BUCKETS: usize = 20;

/// Upper bounds of the latency buckets, in nanoseconds. The resolution is
/// chosen for LLM-turn latencies: fine at the 1-100 ms scale (gateway-internal
/// work) and still diagnostic across the 1-60 s range where upstream queues
/// and streaming turns live (prod hop-ladder: TTFB p50 1.7 s vs 18.4 s).
pub const BOUND_NS: [u64; BUCKETS] = [
    1_000_000,      // 1 ms
    2_500_000,      // 2.5 ms
    5_000_000,      // 5 ms
    10_000_000,     // 10 ms
    25_000_000,     // 25 ms
    50_000_000,     // 50 ms
    100_000_000,    // 100 ms
    250_000_000,    // 250 ms
    500_000_000,    // 500 ms
    1_000_000_000,  // 1 s
    1_500_000_000,  // 1.5 s
    2_000_000_000,  // 2 s
    3_000_000_000,  // 3 s
    5_000_000_000,  // 5 s
    8_000_000_000,  // 8 s
    10_000_000_000, // 10 s
    15_000_000_000, // 15 s
    20_000_000_000, // 20 s
    30_000_000_000, // 30 s
    60_000_000_000, // 60 s
];

/// `le` label text for each bound (seconds) — literal strings, so the
/// exposition never depends on float formatting.
pub const BOUND_LABELS: [&str; BUCKETS] = [
    "0.001", "0.0025", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "1.5", "2",
    "3", "5", "8", "10", "15", "20", "30", "60",
];

// The two arrays describe the same buckets; a mismatch would silently
// mislabel every `le` — enforce it at compile time.
const _: () = assert!(BOUND_NS.len() == BOUND_LABELS.len());

/// Fixed-bucket latency histogram: `counts[i]` holds the observations that
/// landed in bucket `i` (upper bound `BOUND_NS[i]`); observations above the
/// last bound are counted in `count` only (their home is `+Inf`).
pub struct Histogram {
    counts: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicU64,
}

impl Histogram {
    /// All-zero histogram. Not a `const fn`: `[AtomicU64; N]` cannot be
    /// repeated by value (the type has no `Copy`), and routing the repeat
    /// through a named `const` trips `declare_interior_mutable_const`
    /// (clippy::all) — `array::from_fn` is both correct and lint-clean.
    pub fn new() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
        }
    }

    /// Record one observation (nanoseconds). Relaxed ordering: the histogram
    /// is an observability artifact and no invariant depends on cross-thread
    /// ordering of observations.
    pub fn observe(&self, ns: u64) {
        if let Some(i) = BOUND_NS.iter().position(|bound| ns <= *bound) {
            if let Some(slot) = self.counts.get(i) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Prometheus text exposition of one histogram family. Buckets are
    /// rendered cumulative (the format's requirement) from the per-bucket
    /// counters; `le="+Inf"` is the total count.
    pub fn render(&self, out: &mut String, name: &str, help: &str) {
        render_head(out, name, help, "histogram");
        let mut cumulative: u64 = 0;
        for (slot, le) in self.counts.iter().zip(BOUND_LABELS.iter()) {
            cumulative = cumulative.saturating_add(slot.load(Ordering::Relaxed));
            out.push_str(name);
            out.push_str("_bucket{le=\"");
            out.push_str(le);
            out.push_str("\"} ");
            out.push_str(&cumulative.to_string());
            out.push('\n');
        }
        let total = self.count.load(Ordering::Relaxed);
        out.push_str(name);
        out.push_str("_bucket{le=\"+Inf\"} ");
        out.push_str(&total.to_string());
        out.push('\n');
        out.push_str(name);
        out.push_str("_sum ");
        out.push_str(&seconds_text(self.sum_ns.load(Ordering::Relaxed)));
        out.push('\n');
        out.push_str(name);
        out.push_str("_count ");
        out.push_str(&total.to_string());
        out.push('\n');
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// Prometheus text exposition of a scalar gauge family.
pub fn render_gauge(out: &mut String, name: &str, help: &str, value: u64) {
    render_head(out, name, help, "gauge");
    scalar_line(out, name, value);
}

/// Prometheus text exposition of a scalar counter family.
pub fn render_counter(out: &mut String, name: &str, help: &str, value: u64) {
    render_head(out, name, help, "counter");
    scalar_line(out, name, value);
}

fn render_head(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn scalar_line(out: &mut String, name: &str, value: u64) {
    out.push_str(name);
    out.push(' ');
    out.push_str(&value.to_string());
    out.push('\n');
}

/// Nanoseconds as exposition-format seconds: `<int>.<9-digit fraction>`
/// (exact integer math — no float rounding).
fn seconds_text(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let frac = ns % 1_000_000_000;
    let mut out = secs.to_string();
    out.push('.');
    let digits = frac.to_string();
    for _ in digits.len()..9 {
        out.push('0');
    }
    out.push_str(&digits);
    out
}

/// Escape a label value per the exposition format: backslash, double quote
/// and line feed.
pub fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family_value(text: &str, line_prefix: &str) -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(line_prefix))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
    }

    /// Buckets are cumulative and the `+Inf` bucket equals the total count —
    /// the two properties a Prometheus client relies on (a `histogram_quantile`
    /// over a non-cumulative family returns silently wrong percentiles).
    #[test]
    fn histogram_renders_cumulative_buckets_with_inf_total() {
        let h = Histogram::new();
        h.observe(500_000); // <= 1 ms
        h.observe(2_000_000); // <= 2.5 ms
        h.observe(70_000_000_000); // above the last bound -> +Inf only
        let mut out = String::new();
        h.render(&mut out, "atg_test_seconds", "test");
        assert_eq!(
            family_value(&out, "atg_test_seconds_bucket{le=\"0.001\"} "),
            Some(1)
        );
        assert_eq!(
            family_value(&out, "atg_test_seconds_bucket{le=\"0.0025\"} "),
            Some(2)
        );
        assert_eq!(
            family_value(&out, "atg_test_seconds_bucket{le=\"60\"} "),
            Some(2),
            "observations above the last bound stay out of the finite buckets"
        );
        assert_eq!(
            family_value(&out, "atg_test_seconds_bucket{le=\"+Inf\"} "),
            Some(3)
        );
        assert_eq!(h.count(), 3);
        assert_eq!(family_value(&out, "atg_test_seconds_count "), Some(3));
        assert!(
            out.contains("atg_test_seconds_sum 70.002500000"),
            "sum is ns-exact and rendered in seconds: {out}"
        );
    }

    #[test]
    fn seconds_text_is_nanosecond_exact() {
        assert_eq!(seconds_text(0), "0.000000000");
        assert_eq!(seconds_text(1), "0.000000001");
        assert_eq!(seconds_text(1_500_000_000), "1.500000000");
        assert_eq!(seconds_text(3_600_000_000_000), "3600.000000000");
    }

    #[test]
    fn label_values_escape_backslash_quote_and_newline() {
        assert_eq!(escape_label_value("line:atg"), "line:atg");
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
