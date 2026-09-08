// Behavior: non-streaming requests of all three protocols are unpacked into
// turn records containing user input and final model output.
// [Requirement: 协议解包与流式重组；Scenario: 非流式请求解包]
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

async fn post(path: &str, body: &str) {
    let gw = common::stack::gateway_port();
    let req = Request::post(format!("http://127.0.0.1:{gw}{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("request should succeed");
    assert_eq!(resp.status(), 200, "upstream {path} should succeed");
    let _ = resp.collect().await.unwrap();
}

async fn records() -> serde_json::Value {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records endpoint");
    assert_eq!(resp.status(), 200, "records endpoint must be served");
    let text = resp.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&text).expect("records must be JSON")
}

#[tokio::test]
async fn unpack_nonstreaming() {
    start_stack(&[]).await;

    // openai chat completions
    post(
        "/v1/chat",
        r#"{"model":"m","messages":[{"role":"user","content":"chat-user-text"}]}"#,
    )
    .await;
    // anthropic messages
    post(
        "/v1/messages",
        r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"anthropic-user-text"}]}"#,
    )
    .await;
    // openai responses (non-streaming)
    post(
        "/v1/responses",
        r#"{"model":"m","input":"responses-user-text"}"#,
    )
    .await;

    let recs = records().await;
    let arr = recs.as_array().expect("records must be an array");
    assert_eq!(arr.len(), 3, "three turns expected, got {arr:?}");

    let find = |proto: &str| -> serde_json::Value {
        arr.iter()
            .find(|r| r["protocol"] == proto)
            .unwrap_or_else(|| panic!("missing protocol {proto} in {arr:?}"))
            .clone()
    };

    let chat = find("openai_chat");
    assert_eq!(
        chat["user_input"], "chat-user-text",
        "chat user input: {chat}"
    );
    assert!(
        chat["final_output"].as_str().is_some_and(|s| !s.is_empty()),
        "chat output must be extracted: {chat}"
    );

    let anth = find("anthropic_messages");
    assert_eq!(
        anth["user_input"], "anthropic-user-text",
        "anthropic user input: {anth}"
    );
    assert!(
        anth["final_output"].as_str().is_some_and(|s| !s.is_empty()),
        "anthropic output must be extracted: {anth}"
    );

    let resp = find("openai_responses");
    assert_eq!(
        resp["user_input"], "responses-user-text",
        "responses user input: {resp}"
    );
    assert!(
        resp["final_output"].as_str().is_some_and(|s| !s.is_empty()),
        "responses output must be extracted: {resp}"
    );
}
