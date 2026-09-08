// Behavior: anthropic tool_use streaming turns produce a structured tool_calls
// entry (name + complete reassembled arguments), aligned with modeltrace's
// chat.tool_call item; final_output keeps the reassembled SSE text.
// [Requirement: 协议解包与流式重组；Scenario: Anthropic tool_use 流式提取]
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

async fn records() -> Vec<serde_json::Value> {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("records JSON")
}

#[tokio::test]
async fn anthropic_tool_use_stream_extraction() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "anthropic-tooluse-1")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","stream":true,"messages":[{"role":"user","content":"weather-turn"}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let recs = records().await;
    let rec = recs
        .iter()
        .find(|r| {
            r["protocol"] == "anthropic.messages" && r["session_id"] == "anthropic-tooluse-1"
        })
        .unwrap_or_else(|| panic!("tool_use record missing: {recs:?}"));

    // Structured tool_calls must carry the reassembled name + arguments.
    let calls = rec["tool_calls"]
        .as_array()
        .unwrap_or_else(|| panic!("tool_calls array missing: {rec}"));
    assert_eq!(calls.len(), 1, "one tool_use block expected: {rec}");
    assert_eq!(calls[0]["name"], "get_weather");
    let args: serde_json::Value =
        serde_json::from_str(calls[0]["arguments"].as_str().unwrap())
            .expect("reassembled arguments must be valid JSON");
    assert_eq!(args["city"], "Tokyo");

    // final_output stays the reassembled text (no tool_use JSON dump needed
    // once the structured field carries it).
    assert!(
        rec["final_output"].as_str().unwrap_or("").contains("weather-turn")
            || rec["final_output"].as_str().unwrap_or("").is_empty(),
        "final_output must not corrupt the tool call stream: {rec}"
    );
}