// Behavior: gateway transparently forwards HTTP/1.1 and HTTP/2 requests to upstream.
// [Requirement: 透明转发；Scenario: HTTP/1.1 与 HTTP/2 请求透传]
mod common;

use bytes::Bytes;
use common::stack::start_stack;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

fn h2_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http()
}

#[tokio::test]
async fn transparent_forward() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    let body = serde_json::json!({
        "model": "gpt-test",
        "session_id": "sess-fwd-001",
        "messages": [{"role":"user","content":"hi"}]
    });

    // HTTP/1.1 through the gateway.
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = http_client()
        .request(req)
        .await
        .expect("request should reach gateway");
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "gateway must forward, got {status}: {text}"
    );
    assert!(
        text.contains("sess-fwd-001"),
        "upstream response body must pass through unchanged, got: {text}"
    );

    // HTTP/2 prior knowledge through the gateway.
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .version(hyper::Version::HTTP_2)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = h2_client()
        .request(req)
        .await
        .expect("h2c request should reach gateway");
    let status = resp.status();
    let ver = resp.version();
    let text = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "h2c forward failed: {status}: {text}"
    );
    assert_eq!(ver, hyper::Version::HTTP_2, "gateway must preserve HTTP/2");
    assert!(text.contains("sess-fwd-001"), "h2c body mismatch: {text}");
}
