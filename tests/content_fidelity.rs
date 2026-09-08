// Behavior: turn records carry the request/response business content
// verbatim (no redaction, no rewriting) and exclude transport-layer noise
// (per-hop headers, TCP metadata).
// [Requirement: 内容保真；Scenario: 业务内容原样记录；Scenario: 网络传输噪声剥离]
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

fn manifest_dir() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

async fn records(gw: u16) -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("records JSON")
}

#[tokio::test]
async fn content_fidelity() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    // Real claude-cli sample carries a canary marker in the user text.
    let fixture_dir = format!("{}/xtask/harness/fixtures", manifest_dir());
    let body = std::fs::read_to_string(format!(
        "{fixture_dir}/anthropic_messages/claude_cli_request.json"
    ))
    .unwrap();
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("connection", "keep-alive") // transport noise candidate
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let upstream_response = resp.collect().await.unwrap().to_bytes();

    let recs = records(gw).await;
    let rec = recs
        .iter()
        .find(|r| r["protocol"] == "anthropic_messages")
        .unwrap_or_else(|| panic!("anthropic record missing: {recs:?}"));

    // 1. Verbatim: the record must embed the full original request body text,
    // byte for byte — including the canary user text and any secret-looking
    // fields (D4: no redaction at the gateway).
    let raw_req = rec["raw_request"]
        .as_str()
        .unwrap_or_else(|| panic!("raw_request field missing: {rec}"));
    assert_eq!(raw_req, body, "request content must be recorded verbatim");

    let raw_resp = rec["raw_response"]
        .as_str()
        .unwrap_or_else(|| panic!("raw_response field missing: {rec}"));
    assert_eq!(
        raw_resp,
        String::from_utf8_lossy(&upstream_response),
        "response content must be recorded verbatim"
    );

    // 2. Transport noise stripped: no per-hop headers or TCP metadata in the
    // record's serialized form.
    let serialized = serde_json::to_string(rec).unwrap();
    for noise in [
        "\"connection\"",
        "keep-alive",
        "sec-websocket-key",
        "transfer-encoding",
        "tcp_",
        "peer_addr",
    ] {
        assert!(
            !serialized.to_lowercase().contains(&noise.to_lowercase()),
            "transport noise '{noise}' leaked into the record: {serialized}"
        );
    }
}
