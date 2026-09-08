// Behavior: real codex/probe traffic sends bare input items (no `type`
// field); user_input must still be extracted, the turn must not degrade into
// a synthetic session, and the generation child span must carry usage_details
// for the openai.responses shape (input/output/total, no cache buckets).
// [Requirement: 协议解包与流式重组；Scenario: 裸 item 请求的 user_input 与 usage]
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
async fn bare_input_items_extract_user_and_usage() {
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
        .header("content-type", "application/json")
        .header("session-id", "bare-item-sess-1")
        .body(Full::new(Bytes::from(
            // Bare item: no `type` field at all.
            r#"{"model":"m","stream":true,"input":[{"role":"user","content":[{"type":"input_text","text":"bare-item-turn"}]}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let recs = records().await;
    let rec = recs
        .iter()
        .find(|r| r["session_id"] == "bare-item-sess-1")
        .unwrap_or_else(|| panic!("record missing: {recs:?}"));
    assert_eq!(rec["protocol"], "openai.responses");
    // (a) Bare item user extraction: input text must surface.
    assert_eq!(
        rec["user_input"], "bare-item-turn",
        "bare input items must yield the user text: {rec}"
    );
    // (b) Generation child span carries usage_details for the openai shape
    // (input/output/total; no cache buckets — protocol difference, legal).
    let usage = &rec["usage"];
    assert!(
        usage["input_tokens"].is_u64(),
        "usage must be extracted from response.completed: {rec}"
    );
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 22);
    assert_eq!(usage["total_tokens"], 33);
}
