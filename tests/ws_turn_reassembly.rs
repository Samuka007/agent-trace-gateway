// Behavior: one request-response turn on an upgraded WebSocket connection is
// reassembled into a turn record; turn boundaries come from protocol frames,
// not connection lifetime.
// [Requirement: 协议解包与流式重组；Scenario: WebSocket 回合重组]
mod common;

use bytes::Bytes;
use common::stack::start_stack;

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn ws_turn_reassembly() {
    let gw = common::stack::gateway_port();
    start_stack().await;
    let gw = common::stack::gateway_port();

    let url = format!("ws://127.0.0.1:{gw}/ws");
    let req = url.into_client_request().unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("WS handshake through gateway");

    ws.send(Message::Text(
        "{\"type\":\"response.create\",\"client_metadata\":{\"session_id\":\"sess-ws-turn\"}}"
            .into(),
    ))
    .await
    .unwrap();

    let mut completed = false;
    while let Some(msg) = ws.next().await {
        match msg.unwrap() {
            Message::Text(t) => {
                if t.contains("response.tool_call") {
                    ws.send(Message::Text(
                        "{\"type\":\"conversation.item.create\",\"item\":{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":\"ok\"}}".into(),
                    ))
                    .await
                    .unwrap();
                }
                if t.contains("response.completed") {
                    completed = true;
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    assert!(completed, "scripted turn must complete");
    drop(ws);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = Client::builder(TokioExecutor::new())
        .build_http::<Full<Bytes>>()
        .request(req)
        .await
        .unwrap();
    let recs: serde_json::Value =
        serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).unwrap();
    let arr = recs.as_array().expect("records array");
    let rec = arr
        .iter()
        .find(|r| r["protocol"] == "openai_responses_ws")
        .unwrap_or_else(|| panic!("no WS turn record in {arr:?}"));

    // Input comes from the response.create frame.
    assert!(
        rec["user_input"]
            .as_str()
            .unwrap_or("")
            .contains("sess-ws-turn"),
        "WS turn input must come from response.create frame: {rec}"
    );
    // Tool call parsed from the server tool_call frame.
    let tools = rec["tool_calls"].as_array().expect("tool_calls array");
    assert!(
        tools.iter().any(|t| t["name"] == "read_file"
            && t["arguments"].as_str().unwrap_or("").contains("/tmp/x")),
        "tool_call must be parsed from WS frames: {rec}"
    );
    // Output reassembled from delta frames.
    assert_eq!(
        rec["final_output"], "file-content",
        "WS deltas must reassemble: {rec}"
    );
}
