// Behavior: the export flush pool is observable on both health surfaces —
// the in-flight gauge and the effective concurrency knobs appear in the JSON
// that bench/incident scripts read and in the Prometheus exposition a scraper
// reads, under the names those consumers key on.
// [Requirement: ATG#13 A4 导出池可观测；Scenario: health 与 metrics 两格式]
mod common;

use bytes::Bytes;
use common::stack::start_stack;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get_text(path: &str) -> String {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("endpoint");
    assert_eq!(resp.status(), 200, "{path} must be served");
    String::from_utf8(resp.collect().await.unwrap().to_bytes().to_vec()).expect("UTF-8 body")
}

#[tokio::test]
async fn export_pool_gauge_and_knobs_are_on_both_health_surfaces() {
    // The exporter only needs an endpoint to be enabled; this test exports
    // nothing (no traffic), so the listener never has to answer.
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = sink.local_addr().expect("addr").port();

    start_stack(&[
        (
            "ATG_OTLP_ENDPOINT",
            format!("http://127.0.0.1:{port}/api/public/otel"),
        ),
        ("ATG_EXPORT_MAX_INFLIGHT", "3".to_string()),
        ("ATG_EXPORT_WORKERS", "2".to_string()),
    ])
    .await;

    // JSON surface (/__atg/health): the gauge plus the self-attested knobs.
    let health: serde_json::Value =
        serde_json::from_str(&get_text("/__atg/health").await).expect("health JSON");
    assert_eq!(
        health["export_max_inflight"], 3,
        "ATG_EXPORT_MAX_INFLIGHT must reach the health surface: {health}"
    );
    assert_eq!(
        health["export_workers"], 2,
        "ATG_EXPORT_WORKERS must reach the health surface: {health}"
    );
    assert_eq!(
        health["export_inflight"], 0,
        "no traffic ⇒ the pool is empty: {health}"
    );
    assert_eq!(health["export_queue_depth"], 0, "{health}");

    // Prometheus surface (/__atg/metrics).
    let metrics = get_text("/__atg/metrics").await;
    for line in [
        "# TYPE atg_export_inflight gauge",
        "atg_export_inflight 0",
        "atg_export_max_inflight 3",
        "atg_export_workers 2",
        "atg_export_queue_depth 0",
        // The five pre-existing counters must survive the pool change.
        "atg_exported_total 0",
        "atg_export_failed_total 0",
        "atg_export_dropped_total 0",
        "atg_export_panicked_batches_total 0",
    ] {
        assert!(
            metrics.lines().any(|l| l.trim_start().starts_with(line)),
            "metrics exposition is missing {line:?}:\n{metrics}"
        );
    }
}
