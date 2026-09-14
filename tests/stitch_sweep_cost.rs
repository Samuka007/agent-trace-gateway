// Instrument (ATG#5): per-call cost of `PrefixStitcher::assign` as the chain
// table grows. Pre-fix, every call runs a full-table TTL `retain` sweep while
// holding the stitcher mutex — the cost is linear in the table size. Post-fix
// the sweep is amortized (at most once per interval), so the per-call cost is
// flat. Not a CI gate; run explicitly:
//
//   cargo test --test stitch_sweep_cost -- --ignored --nocapture
//
// Only the public API is used, so the same file can be dropped into a
// pre-fix tree to produce the "before" column.
use agent_trace_gateway::trace::prefix::PrefixStitcher;
use std::time::Instant;

fn conversation(head: &str) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"role": "system", "content": "sys"}),
        serde_json::json!({"role": "user", "content": head}),
    ]
}

#[test]
#[ignore = "measurement instrument (ATG#5): run with --ignored --nocapture"]
fn assign_cost_vs_table_size() {
    // The pre-populated table must fit: capacity is read at construction.
    std::env::set_var("ATG_STITCH_CAPACITY", "200000");
    const CALLS: u32 = 200;
    for table in [0usize, 5_000, 20_000, 100_000] {
        let stitcher = PrefixStitcher::new();
        for i in 0..table {
            stitcher.assign("scope", &conversation(&format!("head-{i}")));
        }
        // Steady state: one existing chain being extended (the shape real
        // multi-turn traffic has — same body every call).
        let probe = conversation("probe");
        let (expected, _) = stitcher.assign("scope", &probe);
        let start = Instant::now();
        for _ in 0..CALLS {
            let (session, _) = stitcher.assign("scope", &probe);
            assert_eq!(session, expected, "the live chain must keep extending");
        }
        let elapsed = start.elapsed();
        let per_call_us = elapsed.as_secs_f64() * 1e6 / f64::from(CALLS);
        println!(
            "table={table:>7} chains | {CALLS} assigns | {per_call_us:>10.3} us/call | total {elapsed:?}"
        );
    }
}
