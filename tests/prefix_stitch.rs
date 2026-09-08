// Behavior: requests without any session id are stitched by conversation
// history prefix relations; a same-head request whose history diverges from
// the current chain opens a new segment marked as a compaction breakpoint.
// [Requirement: 会话串联；Scenario: 无标识流量的前缀串联；Scenario: 上下文压缩断点；Scenario: 单发请求]
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

async fn post_chat(body: &str) {
    let gw = common::stack::gateway_port();
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();
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
async fn prefix_stitch() {
    start_stack(&[]).await;

    let fixture_dir = format!(
        "{}/xtask/harness/fixtures/openai.chat_completions",
        manifest_dir()
    );
    // Real omp tool-loop samples: turn4 (5 messages) then turn5 (7 messages).
    // turn5's first 5 messages are byte-identical to turn4 -> strict prefix.
    for name in ["omp_tool_turn4.json", "omp_tool_turn5.json"] {
        let body = std::fs::read_to_string(format!("{fixture_dir}/{name}")).unwrap();
        post_chat(&body).await;
    }
    // turn3 has a DIFFERENT system+first-user head than turn4/turn5, so it is
    // its own conversation (new session, no breakpoint mark).
    let turn3 = std::fs::read_to_string(format!("{fixture_dir}/omp_tool_turn3.json")).unwrap();
    post_chat(&turn3).await;
    // Compaction simulation: same head as turn4/turn5 but a SHORTER history
    // (turn5 minus its last message). Must open a new segment with the
    // breakpoint mark.
    let turn5: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{fixture_dir}/omp_tool_turn5.json")).unwrap(),
    )
    .unwrap();
    let mut compacted = turn5.clone();
    compacted["messages"].as_array_mut().unwrap().pop();
    post_chat(&compacted.to_string()).await;
    // Single-shot unrelated request.
    post_chat(r#"{"model":"m","messages":[{"role":"user","content":"one-shot unrelated"}]}"#).await;

    let recs = records().await;
    let chat: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "openai.chat_completions")
        .collect();
    assert_eq!(
        chat.len(),
        5,
        "five openai.chat_completions records expected: {recs:?}"
    );

    let s4 = chat[0]["session_id"].as_str().unwrap_or("");
    let s5 = chat[1]["session_id"].as_str().unwrap_or("");
    let s3 = chat[2]["session_id"].as_str().unwrap_or("");
    let sc = chat[3]["session_id"].as_str().unwrap_or("");
    let s1 = chat[4]["session_id"].as_str().unwrap_or("");

    // No explicit id anywhere: synthetic prefix fingerprints.
    for (i, s) in [s4, s5, s3, sc, s1].iter().enumerate() {
        assert!(
            s.starts_with("pfx:"),
            "record {i} must carry a synthetic prefix session, got '{s}'"
        );
    }
    // Strict prefix pair shares one session.
    assert_eq!(s4, s5, "prefix turns must share one session: {s4} vs {s5}");
    // Different head -> its own conversation.
    assert_ne!(s3, s4, "different-head request must not merge");
    // Same-head shortened history -> new segment, breakpoint marked.
    assert_ne!(sc, s5, "compacted history must not merge into the chain");
    assert_eq!(
        chat[3]["breakpoint"], true,
        "compacted segment must carry the breakpoint mark: {:?}",
        chat[3]
    );
    // Single-shot is independent.
    assert_ne!(s1, s4);
    assert_ne!(s1, s3);
    assert_ne!(s1, sc);
    // No breakpoint for a pure extension or a fresh head.
    assert_ne!(
        chat[0]["breakpoint"], true,
        "chain head is not a breakpoint"
    );
    assert_ne!(
        chat[1]["breakpoint"], true,
        "prefix extension is not a breakpoint"
    );
}
