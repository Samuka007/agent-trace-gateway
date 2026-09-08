// Behavior: WebSocket upgraded connections pass through the gateway both ways.
// [Requirement: 透明转发；Scenario: WebSocket 升级连接透传]
mod common;

use common::stack::start_stack;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn ws_passthrough() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    let url = format!("ws://127.0.0.1:{gw}/ws");
    let req = url.into_client_request().unwrap();
    let (mut ws, resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("WS handshake through gateway");
    assert_eq!(resp.status(), 101, "gateway must pass the 101 upgrade");

    // Send response.create (client -> upstream direction).
    ws.send(Message::Text(
        "{\"type\":\"response.create\",\"client_metadata\":{\"session_id\":\"sess-ws-test\"}}"
            .into(),
    ))
    .await
    .expect("client frame send");

    // Expect scripted fixture turn: tool_call -> (we reply tool result) -> delta -> completed.
    let mut frames = Vec::new();
    let mut got_tool_call = false;
    while let Some(msg) = ws.next().await {
        match msg.expect("frame should be ok") {
            Message::Text(t) => {
                if t.contains("response.output_item.done") && !got_tool_call {
                    got_tool_call = true;
                    ws.send(Message::Text(
                        "{\"type\":\"conversation.item.create\",\"item\":{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":\"ok\"}}".into(),
                    ))
                    .await
                    .expect("tool result send");
                }
                frames.push(t);
                if frames
                    .last()
                    .map(|f| f.contains("response.done"))
                    .unwrap_or(false)
                {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    assert!(
        got_tool_call,
        "tool_call frame must traverse gateway: {frames:?}"
    );
    assert!(
        frames.iter().any(|f| f.contains("response.done")),
        "completed frame must traverse gateway: {frames:?}"
    );
    assert!(
        frames.len() >= 3,
        "all scripted frames expected, got {}: {frames:?}",
        frames.len()
    );
}
