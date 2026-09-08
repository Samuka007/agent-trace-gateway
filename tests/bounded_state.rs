// Behavior: prefix stitch state is bounded (LRU capacity + TTL). Sessions
// beyond the limits degrade to independent single-turn records; no errors are
// produced; state memory scales with turn count, not history bytes.
// [Requirement: 会话串联；Scenario: 串联状态有界；Scenario: 网关重启后的串联状态]
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

async fn post_chat(body: &str) {
    let gw = common::stack::gateway_port();
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(
        resp.status(),
        200,
        "bounded-state degradation must not error"
    );
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

fn chat_body(system: &str, user: &str) -> String {
    serde_json::json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn bounded_stitch_state() {
    // Capacity 2 chains, TTL 300ms — small on purpose to exercise eviction.
    start_stack(&[
        ("ATG_STITCH_CAPACITY", "2".to_string()),
        ("ATG_STITCH_TTL_MS", "300".to_string()),
    ])
    .await;

    // Three distinct conversations against capacity 2.
    post_chat(&chat_body("sys-A", "user-A")).await;
    post_chat(&chat_body("sys-B", "user-B")).await;
    post_chat(&chat_body("sys-C", "user-C")).await;
    // Head A was the first inserted; with capacity 2 it is now evicted.
    // Re-sending A must NOT error and must NOT merge with the surviving chains.
    post_chat(&chat_body("sys-A", "user-A")).await;

    let recs = records().await;
    let chat: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "openai.chat_completions")
        .collect();
    assert_eq!(chat.len(), 4);

    let ids: Vec<&str> = chat
        .iter()
        .map(|r| r["session_id"].as_str().unwrap_or(""))
        .collect();
    // Every record still carries a synthetic session (degraded to independent
    // turn, not an error).
    for (i, id) in ids.iter().enumerate() {
        assert!(id.starts_with("pfx:"), "record {i} lost its session: {id}");
    }
    // B and C kept their chains (most recent two); A's re-send after eviction
    // got a fresh session distinct from B/C.
    // B must equal the first B occurrence if still alive; we only assert the
    // evicted re-send is independent from the surviving chains.
    assert_ne!(
        ids[3], ids[0],
        "evicted head must reopen a fresh chain, not merge with its old session"
    );
    assert_ne!(
        ids[3], ids[1],
        "evicted head must not merge into surviving chain B"
    );
    assert_ne!(
        ids[3], ids[2],
        "evicted head must not merge into surviving chain C"
    );

    // TTL: wait out the TTL, re-send A again — expired chain must open a new
    // session, still without error.
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    post_chat(&chat_body("sys-A", "user-A")).await;
    let recs = records().await;
    let chat: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "openai.chat_completions")
        .collect();
    assert_eq!(chat.len(), 5);
    let ids: Vec<&str> = chat
        .iter()
        .map(|r| r["session_id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        ids[4].starts_with("pfx:"),
        "TTL-expired re-send must degrade to a fresh session, not error"
    );
    assert_ne!(ids[4], ids[3], "expired chain must not continue silently");

    // Memory scaling: a single very large history still produces exactly one
    // record and the gateway stays responsive (state is per-message
    // fingerprints, not history bytes).
    let big_content = "x".repeat(1_000_000);
    let big = serde_json::json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "big-sys"},
            {"role": "user", "content": big_content},
            {"role": "assistant", "content": "ack"},
            {"role": "user", "content": "follow-up"}
        ]
    })
    .to_string();
    let start = std::time::Instant::now();
    post_chat(&big).await;
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "large history must not stall the gateway"
    );
}
