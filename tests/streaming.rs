// Behavior: gateway streams SSE responses without buffering the whole stream.
// [Requirement: 透明转发；Scenario: 流式响应不缓冲]
mod common;

use common::stack::start_stack;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Instant;

#[tokio::test]
async fn streaming_passthrough() {
    let gw = common::stack::gateway_port();
    start_stack().await;
    let gw = common::stack::gateway_port();

    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/sse"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from("{\"stream\":true}")))
        .unwrap();
    let resp = Client::builder(TokioExecutor::new())
        .build_http::<Full<Bytes>>()
        .request(req)
        .await
        .expect("SSE request should reach gateway");
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").starts_with("text/event-stream"))
        .unwrap_or(false));

    let mut stream = resp.into_data_stream();
    let start = Instant::now();
    let mut arrivals = Vec::new();
    let mut received = Vec::new();
    while let Some(chunk) = stream.next().await {
        let c = chunk.expect("SSE chunk should be ok");
        arrivals.push(start.elapsed());
        received.extend_from_slice(&c);
    }
    let text = String::from_utf8_lossy(&received).to_string();

    // Fixture sends 3 events with 20ms gaps. If the gateway buffered the whole
    // response, every chunk would arrive together (span ~0). Streaming means
    // the arrival span covers at least one fixture gap.
    assert!(!arrivals.is_empty(), "no chunks received");
    assert!(
        text.contains("response.completed"),
        "stream incomplete: {text}"
    );
    if arrivals.len() >= 2 {
        let span = *arrivals.last().unwrap() - *arrivals.first().unwrap();
        assert!(
            span >= std::time::Duration::from_millis(15),
            "all chunks arrived within {span:?}: gateway appears to buffer the stream"
        );
    }
}
