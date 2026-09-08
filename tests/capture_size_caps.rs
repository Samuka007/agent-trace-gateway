// Behavior: captured content is bounded by configured byte caps with a
// deterministic truncation marker carrying original and captured byte counts.
// [Requirement: 内容保真；Scenario: 记录体积上限]
mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn records(gw: u16) -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("records JSON")
}

#[tokio::test]
async fn capture_size_caps() {
    // Tiny cap so truncation is observable without huge payloads.
    common::stack::start_stack_with_env(&[("ATG_CAPTURE_MAX_BYTES", "1024".to_string())]).await;
    let gw = common::stack::gateway_port();

    // Oversized request body (~100KB user text).
    let big_content = format!("{}TAIL-MARKER-END-{}", "A".repeat(99_980), "ZZZ");
    let body = serde_json::json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "cap-sys"},
            {"role": "user", "content": big_content}
        ]
    })
    .to_string();
    let original_bytes = body.len();
    let original = body.clone();
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let recs = records(gw).await;
    let rec = recs
        .iter()
        .find(|r| r["protocol"] == "openai_chat")
        .unwrap_or_else(|| panic!("chat record missing: {recs:?}"));

    let raw = rec["raw_request"].as_str().expect("raw_request");
    // Bounded: captured bytes must not exceed the cap plus the marker.
    assert!(
        raw.len() <= 1024 + 128,
        "raw_request exceeds cap: {} bytes",
        raw.len()
    );
    // Deterministic truncation marker with original and captured counts.
    assert!(
        raw.contains("[truncated:original_bytes="),
        "truncation marker missing: {raw}"
    );
    let expected_marker = format!("[truncated:original_bytes={original_bytes},captured_bytes=");
    assert!(
        raw.contains(&expected_marker),
        "marker must carry the true original byte count ({original_bytes}): {raw}"
    );
    // Content before truncation is the original verbatim prefix (no rewriting).
    assert!(
        raw.starts_with(&original[..1000]),
        "truncation must keep the verbatim prefix"
    );
    // Oversized tail must NOT be present.
    assert!(
        !raw.contains(&big_content[big_content.len() - 100..]),
        "oversized tail leaked"
    );
}
