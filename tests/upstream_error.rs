// Behavior: upstream errors pass through the gateway to the client.
// [Requirement: 透明转发；Scenario: 上游错误透传]
mod common;

use bytes::Bytes;
use common::stack::start_stack;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

#[tokio::test]
async fn upstream_error_passthrough() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    // 404 from upstream must pass through with its body.
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/unknown"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from("{}")))
        .unwrap();
    let resp = Client::builder(TokioExecutor::new())
        .build_http::<Full<Bytes>>()
        .request(req)
        .await
        .expect("request should reach gateway");
    let status = resp.status();
    let text = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "upstream 404 must pass through, got {status}"
    );
    assert!(
        text.contains("not found"),
        "upstream error body lost: {text}"
    );

    // G3: an HTTP-level failure on a protocol-detected path must mark the
    // turn errored — the record exports status ERROR instead of a silently
    // "successful" turn. /v1/messages/count_tokens detects anthropic
    // (prefix) while the fixture answers 404 (unknown route).
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","messages":[{"role":"user","content":"g3-probe"}]}"#,
        )))
        .unwrap();
    let resp = Client::builder(TokioExecutor::new())
        .build_http::<Full<Bytes>>()
        .request(req)
        .await
        .expect("request should reach gateway");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let _ = resp.collect().await;
    // The logging hook runs asynchronously off the response path — poll
    // briefly for the record to land.
    let mut g3: Option<serde_json::Value> = None;
    for _ in 0..20 {
        let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = Client::builder(TokioExecutor::new())
            .build_http::<Full<Bytes>>()
            .request(req)
            .await
            .expect("records");
        let recs: Vec<serde_json::Value> =
            serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json");
        g3 = recs
            .into_iter()
            .find(|r| r["protocol"] == "anthropic.messages" && r["user_input"] == "g3-probe");
        if g3.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let g3 = g3.unwrap_or_else(|| panic!("g3 record never appeared"));
    assert_eq!(
        g3["error"], "http_status: 404",
        "HTTP-level failure must mark the turn errored: {g3}"
    );

    // Connection refused upstream: gateway must answer with a 5xx, not hang.
    let gw_port = gw + 100;
    let listen = format!("127.0.0.1:{gw_port}");
    let dead_upstream = format!("127.0.0.1:{}", common::stack::fixture_port() + 500);
    let thread_listen = listen.clone();
    std::thread::spawn(move || {
        agent_trace_gateway::gateway_app::run(&thread_listen, &dead_upstream);
    });
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&listen).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let req = Request::post(format!("http://{listen}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from("{}")))
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        Client::builder(TokioExecutor::new())
            .build_http::<Full<Bytes>>()
            .request(req),
    )
    .await;
    let resp = result
        .expect("gateway must respond when upstream is down, not hang")
        .expect("response expected");
    let status = resp.status();
    assert!(
        status.is_server_error(),
        "dead upstream must yield 5xx, got {status}"
    );
}
