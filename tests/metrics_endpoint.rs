// Behavior: /__atg/metrics serves Prometheus text carrying the
// self-attestation info metric, the queue/backpressure gauges and the
// per-stage latency histograms; served traffic moves them, histogram buckets
// stay cumulative and `le="+Inf"` equals the total count (the two properties
// a Prometheus server's histogram_quantile silently depends on).
// [Requirement: ATG#2 排队/耗时面；Scenario: metrics 端点形状与计数]
mod common;

use bytes::Bytes;
use common::stack::start_stack;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The reported variant is a compile-time property; this test runs under
/// both feature selections and pins the correspondence.
#[cfg(feature = "bench-trace-mode")]
const EXPECTED_VARIANT: &str = "bench";
#[cfg(not(feature = "bench-trace-mode"))]
const EXPECTED_VARIANT: &str = "prod";

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Value of the (unlabelled) sample line named `name` (at the start of the
/// line, followed by a single space — so `a` never matches `a_b`).
fn scalar(body: &str, name: &str) -> u64 {
    body.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|rest| rest.strip_prefix(' ')))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("metric {name} missing/invalid in:\n{body}"))
}

async fn metrics() -> (String, String) {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/metrics"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("metrics endpoint");
    assert_eq!(resp.status(), 200, "metrics endpoint must be served");
    let ctype = resp
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    (ctype, body)
}

#[tokio::test]
async fn metrics_surface_attests_and_counts_traffic() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    for i in 0..3 {
        let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"metrics-turn-{i}"}}]}}"#
            ))))
            .unwrap();
        let resp = client().request(req).await.expect("request served");
        assert_eq!(resp.status(), 200);
        let _ = resp.collect().await.unwrap();
    }
    // Turns record from logging() — the same hook that feeds the gauges and
    // the stage histograms, so a visible turn means the observation happened.
    // One of the turns is stitch-eligible (anthropic, two messages) so the
    // prefix-stitch families have something to report (ATG#5).
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/messages"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","messages":[{"role":"system","content":"sys-stitch"},{"role":"user","content":"user-stitch"}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request served");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let mut recorded = 0;
    for _ in 0..20 {
        let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = client().request(req).await.expect("records");
        let recs: Vec<serde_json::Value> =
            serde_json::from_slice(&resp.collect().await.unwrap().to_bytes())
                .expect("records JSON");
        recorded = recs.len();
        if recorded >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(recorded >= 3, "three turns must record, got {recorded}");

    let (ctype, body) = metrics().await;
    assert!(
        ctype.starts_with("text/plain"),
        "metrics must be Prometheus text, got content-type {ctype:?}"
    );

    // Self-attestation (ATG issue #2): build identity rides the info metric.
    let info = format!(
        "atg_info{{version=\"{}\",variant=\"{EXPECTED_VARIANT}\",trace_mode=\"full\",trace_tag=\"",
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        body.contains(&info),
        "atg_info must self-attest the build: {body}"
    );

    // Queue/backpressure (ATG issue #2): gauges present, drained after the
    // turns (the scrape counts itself: at most 1), high-water moved, and the
    // effective worker count is reported. This stack sets no
    // ATG_WORKER_THREADS => the default 1 (ATG#1 Specification 5); the env
    // read itself is nailed by tests/worker_threads_env.rs.
    let inflight = scalar(&body, "atg_requests_inflight");
    assert!(inflight <= 1, "inflight must release: {body}");
    assert!(
        scalar(&body, "atg_requests_inflight_high_water") >= 1,
        "high-water must have observed the turns: {body}"
    );
    assert_eq!(scalar(&body, "atg_worker_threads"), 1, "{body}");

    // Counters follow the traffic.
    assert!(scalar(&body, "atg_turns_total") >= 3, "{body}");

    // Prefix-stitch state (ATG#5): the stitched conversation registered, the
    // capacity is the configured bound, and nothing has been dropped.
    assert!(scalar(&body, "atg_stitch_entries") >= 1, "{body}");
    assert_eq!(scalar(&body, "atg_stitch_capacity"), 100000, "{body}");
    assert_eq!(scalar(&body, "atg_stitch_expired_total"), 0, "{body}");
    assert_eq!(scalar(&body, "atg_stitch_evicted_total"), 0, "{body}");
    // Lock attribution (ATG#5): the timed critical sections are exposed.
    assert!(scalar(&body, "atg_stitch_hold_ns_total") > 0, "{body}");
    assert!(scalar(&body, "atg_store_hold_ns_total") > 0, "{body}");

    // Per-stage timing (ATG issue #2): every served turn observes the four
    // stages, and the bucket series are cumulative with +Inf = count.
    for family in [
        "atg_stage_wait_upstream_seconds",
        "atg_stage_time_to_first_byte_seconds",
        "atg_stage_delivery_seconds",
        "atg_stage_finalize_seconds",
    ] {
        let count = scalar(&body, &format!("{family}_count"));
        assert!(count >= 3, "{family} must observe the turns: {body}");
        let inf = scalar(&body, &format!("{family}_bucket{{le=\"+Inf\"}}"));
        assert_eq!(
            inf, count,
            "{family}: +Inf bucket must equal the count: {body}"
        );
        let le_last = scalar(&body, &format!("{family}_bucket{{le=\"60\"}}"));
        assert!(
            le_last <= count,
            "{family}: finite buckets cannot exceed the total: {body}"
        );
    }
    // In-gateway TTFB is the metric the hop ladder cannot attribute — it must
    // be strictly positive for a real turn (>= 1 ns is trivially true; the
    // sum proves an observation was actually added).
    assert!(
        !body.contains("atg_stage_time_to_first_byte_seconds_sum 0.000000000"),
        "TTFB sum must be non-zero after real turns: {body}"
    );
}
