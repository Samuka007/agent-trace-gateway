// Behavior: /__atg/health self-attests the build (version / variant /
// trace_mode), carries the queue/backpressure gauges, and those gauges
// release when a turn completes — the new_ctx -> logging bracket pingora
// guarantees must balance, or every scrape reports a growing backlog that
// does not exist.
// [Requirement: ATG#2 health 自证 + 排队面；Scenario: health 字段与 gauge 释放]
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

async fn get_json(path: &str) -> serde_json::Value {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("endpoint");
    assert_eq!(resp.status(), 200, "{path} must be served");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("JSON body")
}

#[tokio::test]
async fn health_attests_build_and_releases_gauges() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","messages":[{"role":"user","content":"health-probe-turn"}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request served");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    // The record is finalized inside logging() — its visibility proves the
    // turn already left the inflight/awaiting sets.
    let mut recs = Vec::new();
    for _ in 0..20 {
        recs = get_json("/__atg/records")
            .await
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !recs.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!recs.is_empty(), "the turn must record");

    let h = get_json("/__atg/health").await;
    // Self-attestation (ATG issue #2).
    assert_eq!(h["version"], env!("CARGO_PKG_VERSION"), "{h}");
    assert_eq!(h["variant"], EXPECTED_VARIANT, "{h}");
    assert_eq!(h["trace_mode"], "full", "{h}");
    // Queue/backpressure (ATG issue #2): the effective worker count and the
    // gauges. The health request counts itself (it is inside the gateway
    // while it renders), so a drained gateway reports at most 1; the turn
    // must not still be counted (2+ would mean the bracket leaked).
    assert_eq!(h["worker_threads"], 8, "{h}");
    let inflight = h["inflight"].as_u64().expect("inflight is a number");
    assert!(inflight <= 1, "inflight must release with the request: {h}");
    assert!(
        h["inflight_high_water"].as_u64().unwrap_or(0) >= 1,
        "the served turn must have been counted: {h}"
    );
    let awaiting = h["awaiting_upstream"].as_u64().expect("number");
    assert!(
        awaiting <= 1,
        "no served turn may still be waiting on the upstream: {h}"
    );
    assert!(
        h["turns_total"].as_u64().unwrap_or(0) >= 1,
        "the turn must be counted: {h}"
    );
    // Export is disabled in the test stack: the queue probe reports its
    // shape without inventing occupancy.
    assert_eq!(h["export_queue_depth"], 0, "{h}");
}
