// Behavior: export failures never block or slow business requests. With the
// export endpoint permanently failing, requests still complete and the failure
// is counted (fail-open).
// [Requirement: 轨迹导出与故障恢复；Scenario: 导出故障 fail-open]
mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder;
use std::time::Instant;
use tokio::net::TcpListener;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// A collector that always answers 500 (persistent export failure).
async fn failing_collector(port: u16) {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper::service::service_fn(
                    |_req: hyper::Request<hyper::body::Incoming>| async move {
                        // Simulate latency then fail.
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        let mut r = hyper::Response::new(Full::new(Bytes::from("boom")));
                        *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        Ok::<_, std::convert::Infallible>(r)
                    },
                );
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
}

#[tokio::test]
async fn export_fail_open() {
    let collector_port = common::stack::fixture_port() + 200;
    failing_collector(collector_port).await;

    common::stack::start_stack(&[(
        "ATG_OTLP_ENDPOINT",
        format!("http://127.0.0.1:{collector_port}/api/public/otel"),
    )])
    .await;
    let gw = common::stack::gateway_port();

    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "failopen-turn"}]
    });

    // Multiple requests; each must complete quickly despite the failing,
    // 1.5s-latency export endpoint.
    for i in 0..3 {
        let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", format!("failopen-{i}"))
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let start = Instant::now();
        let resp = client().request(req).await.expect("request must succeed");
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), StatusCode::OK, "business request must pass");
        let _ = resp.collect().await.unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(400),
            "request took {elapsed:?}: export failure is blocking business traffic"
        );
    }

    // The records store still captured the turns (fail-open drops export, not
    // capture).
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    let recs: Vec<serde_json::Value> =
        serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        recs.len(),
        3,
        "all turns must be captured despite export failure"
    );

    // The failure must be OBSERVABLE via the gateway health endpoint: the
    // failing export increments the `failed` counter (never blocks traffic).
    let mut observed_failed = 0u64;
    for _ in 0..50 {
        let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/health"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = client().request(req).await.expect("health");
        let health: serde_json::Value =
            serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).unwrap();
        observed_failed = health["failed"].as_u64().unwrap_or(0);
        if observed_failed > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        observed_failed > 0,
        "export failure must be recorded as an observable health counter (failed={observed_failed})"
    );
}
