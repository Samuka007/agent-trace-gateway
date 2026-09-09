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

async fn post_anthropic(body: &str) {
    let gw = common::stack::gateway_port();
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/messages"))
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

    // ---- Part A: chat exits the stitcher (USER RULING, F3) -------------
    // openai.chat_completions SDK traffic is stateless single-shot — no
    // session semantics, so no synthetic session may be minted from
    // accidental prefix sharing (previously these stitched).
    let fixture_dir = format!(
        "{}/xtask/harness/fixtures/openai.chat_completions",
        manifest_dir()
    );
    for name in [
        "omp_tool_turn4.json",
        "omp_tool_turn5.json",
        "omp_tool_turn3.json",
    ] {
        let body = std::fs::read_to_string(format!("{fixture_dir}/{name}")).unwrap();
        post_chat(&body).await;
    }
    post_chat(r#"{"model":"m","messages":[{"role":"user","content":"one-shot unrelated"}]}"#).await;
    let recs = records().await;
    let chat: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "openai.chat_completions")
        .collect();
    assert_eq!(chat.len(), 4, "four chat records expected: {recs:?}");
    for (i, r) in chat.iter().enumerate() {
        assert!(
            r["session_id"].as_str().unwrap_or("").is_empty(),
            "chat record {i} must NOT get a synthetic session (user ruling): {r}"
        );
        assert_ne!(
            r["breakpoint"], true,
            "no stitcher run -> no breakpoint: {r}"
        );
        assert_ne!(
            r["session_synthetic"], true,
            "chat records must not be flagged synthetic: {r}"
        );
    }

    // ---- Part B: anthropic.messages still stitches --------------------
    // Same prefix relations as the original test, on a protocol WITH
    // session semantics (multi-turn replay).
    let a = r#"{"model":"m","messages":[{"role":"system","content":"st-sys"},{"role":"user","content":"st-u1"}]}"#;
    let b = r#"{"model":"m","messages":[{"role":"system","content":"st-sys"},{"role":"user","content":"st-u1"},{"role":"assistant","content":"ok"},{"role":"user","content":"st-u2"}]}"#;
    let c = r#"{"model":"m","messages":[{"role":"system","content":"other-sys"},{"role":"user","content":"st-u1"}]}"#;
    // Compaction: same head, history shorter than the chain end (a again).
    let single = r#"{"model":"m","messages":[{"role":"user","content":"one-shot"}]}"#;
    post_anthropic(a).await;
    post_anthropic(b).await;
    post_anthropic(c).await;
    post_anthropic(a).await;
    post_anthropic(single).await;

    let recs = records().await;
    let anth: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "anthropic.messages")
        .collect();
    assert_eq!(anth.len(), 5, "five anthropic records expected: {recs:?}");
    let sid = |i: usize| anth[i]["session_id"].as_str().unwrap_or("").to_string();
    // Strict prefix pair shares one synthetic session; different head gets
    // its own; compaction opens a new segment with the breakpoint mark.
    assert!(
        sid(0).starts_with("pfx:"),
        "head mints a session: {:#?}",
        anth[0]
    );
    assert_eq!(sid(0), sid(1), "prefix turns must share one session");
    assert_ne!(sid(2), sid(0), "different-head request must not merge");
    assert_ne!(
        sid(3),
        sid(1),
        "compacted history must not merge into the chain"
    );
    assert_eq!(
        anth[3]["breakpoint"], true,
        "compacted segment must carry the breakpoint mark: {:#?}",
        anth[3]
    );
    // F3 chain->=2 gate: a single-message request cannot evidence
    // continuity — no synthetic session (previously minted a fresh one).
    assert!(
        sid(4).is_empty(),
        "single-message request must not mint a session: {:#?}",
        anth[4]
    );
    // Synthetic sessions are flagged on the wire-boundary record.
    for i in [0usize, 1, 2, 3] {
        assert_eq!(
            anth[i]["session_synthetic"], true,
            "stitched record {i} must be flagged synthetic"
        );
    }
}
