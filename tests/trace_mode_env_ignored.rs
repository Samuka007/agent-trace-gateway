// Invariant (v0.3.12, ATG issue #3): a PRODUCTION build (default features)
// does not read ATG_TRACE_MODE at all — with the variable set to "off" in
// the environment the gateway still captures and parses turns, and its
// self-attestation reports capture enabled (`trace_mode: "full"`,
// `variant: "prod"`). Capture-off exists only under a compile-time feature;
// the bench build flips both fields (see tests/trace_mode.rs).
// [Requirement: capture-off 仅 bench；Scenario: 生产构建下 env 被忽略]
#![cfg(not(feature = "bench-trace-mode"))]

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

async fn get_json(path: &str) -> serde_json::Value {
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("endpoint");
    assert_eq!(resp.status(), 200, "{path} must be served");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("JSON body")
}

/// The stack starts with `ATG_TRACE_MODE=off` in the process environment
/// (process-global, set before the gateway thread starts) — exactly the
/// deployment misconfiguration this invariant must neutralize.
#[tokio::test]
async fn prod_build_ignores_atg_trace_mode_env() {
    start_stack(&[("ATG_TRACE_MODE", "off".to_string())]).await;
    let gw = common::stack::gateway_port();

    // Self-attestation first: the binary reports what it does, not what the
    // environment says.
    let h = get_json("/__atg/health").await;
    assert_eq!(
        h["trace_mode"], "full",
        "production build must ignore ATG_TRACE_MODE: {h}"
    );
    assert_eq!(h["variant"], "prod", "default-feature build is prod: {h}");
    assert_eq!(h["version"], env!("CARGO_PKG_VERSION"), "{h}");

    // A real turn: the response passes through AND the turn is captured and
    // parsed — the three observables the capture-off mode removes.
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","messages":[{"role":"user","content":"env-off-invariant"}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request served");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let mut recs = Vec::new();
    for _ in 0..20 {
        recs = get_json("/__atg/records")
            .await
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !recs.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let rec = recs
        .iter()
        .find(|r| r["protocol"] == "openai.chat_completions")
        .unwrap_or_else(|| panic!("turn record missing: {recs:?}"));
    assert_eq!(
        rec["user_input"], "env-off-invariant",
        "request parsing must be active: {rec}"
    );
    assert!(
        rec["final_output"]
            .as_str()
            .unwrap_or("")
            .contains("echo:env-off-invariant"),
        "response parsing must be active: {rec}"
    );
    assert!(
        !rec["raw_request"].as_str().unwrap_or("").is_empty(),
        "raw capture must be active (a shell record has no raw bytes): {rec}"
    );
}
