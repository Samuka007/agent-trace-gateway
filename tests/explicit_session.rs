// Behavior: requests carrying the same explicit session id are recorded under
// one session trajectory; extraction follows the production priority
// (body metadata.user_id envelope / client_metadata.session_id, header
// X-Claude-Code-Session-Id / session-id).
// [Requirement: 会话串联；Scenario: 显式会话标识串联]
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

async fn post_with_session(path: &str, body: &str, session_header: Option<&str>) {
    let gw = common::stack::gateway_port();
    let mut req = Request::post(format!("http://127.0.0.1:{gw}{path}"))
        .header("content-type", "application/json");
    if let Some(h) = session_header {
        req = req.header("x-claude-code-session-id", h);
    }
    let req = req.body(Full::new(Bytes::from(body.to_string()))).unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();
}

async fn post_raw(path: &str, body: &str, headers: &[(&str, &str)]) {
    let gw = common::stack::gateway_port();
    let mut req = Request::post(format!("http://127.0.0.1:{gw}{path}"))
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Full::new(Bytes::from(body.to_string()))).unwrap();
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
async fn explicit_session_stitch() {
    start_stack(&[]).await;

    // Real claude-cli samples: three turns of session 01a01f21-eae3-7000-9857-78f64c4de4cc,
    // session id inside metadata.user_id JSON envelope (no header on these).
    let fixture_dir = format!("{}/xtask/harness/fixtures", manifest_dir());
    for name in ["claude_cli_request.json"] {
        let body =
            std::fs::read_to_string(format!("{fixture_dir}/anthropic.messages/{name}")).unwrap();
        post_with_session("/v1/messages", &body, None).await;
    }
    // Header-only session (X-Claude-Code-Session-Id), no body envelope.
    post_with_session(
        "/v1/messages",
        r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hdr-turn"}]}"#,
        Some("01a01f21-eae3-7000-9857-78f64c4de4cc"),
    )
    .await;

    // Real codex sample: session id in body client_metadata.session_id.
    let codex_body =
        std::fs::read_to_string(format!("{fixture_dir}/openai.responses/codex_turn1.json"))
            .unwrap();
    post_with_session("/v1/responses", &codex_body, None).await;

    let recs = records().await;

    // claude-cli envelope sessions + header session must share one session id.
    // (Scoped by session id: tests in this binary share one gateway's record
    // store, so sibling tests' anthropic turns would otherwise be counted.)
    let claude_recs: Vec<_> = recs
        .iter()
        .filter(|r| {
            r["protocol"] == "anthropic.messages"
                && r["session_id"] == "01a01f21-eae3-7000-9857-78f64c4de4cc"
        })
        .collect();
    assert_eq!(
        claude_recs.len(),
        2,
        "two anthropic turns expected: {recs:?}"
    );
    for r in &claude_recs {
        assert_eq!(
            r["session_id"], "01a01f21-eae3-7000-9857-78f64c4de4cc",
            "claude-cli session must be extracted (body envelope or header): {r}"
        );
    }

    // codex client_metadata session.
    let codex_rec = recs
        .iter()
        .find(|r| {
            r["protocol"] == "openai.responses"
                && r["session_id"] == "01a01f1f-bcff-7c80-94a1-9bbbc9fe9145"
        })
        .unwrap_or_else(|| panic!("codex record missing: {recs:?}"));
    assert_eq!(
        codex_rec["session_id"], "01a01f1f-bcff-7c80-94a1-9bbbc9fe9145",
        "codex client_metadata.session_id must be extracted: {codex_rec}"
    );
}

/// Source 6 end-to-end: legacy `metadata.user_id` regex captures the session
/// uuid on the anthropic protocol.
#[tokio::test]
async fn legacy_user_id_session_extraction() {
    start_stack(&[]).await;
    let uuid = "1234abcd-12ab-34cd-56ef-1234567890ab";
    let hex64 = "0123456789abcdef".repeat(4);
    let body = format!(
        r#"{{"model":"m","max_tokens":8,"metadata":{{"user_id":"user_{hex64}_account_abc_session_{uuid}"}},"messages":[{{"role":"user","content":"legacy-turn"}}]}}"#
    );
    post_raw("/v1/messages", &body, &[]).await;

    let recs = records().await;
    let rec = recs
        .iter()
        // Scoped by this test's session: sibling tests in this binary share
        // one gateway's record store, and their anthropic turns (with other
        // session ids) may appear first under concurrent scheduling.
        .find(|r| r["protocol"] == "anthropic.messages" && r["session_id"] == uuid)
        .unwrap_or_else(|| panic!("anthropic record missing: {recs:?}"));
    assert_eq!(
        rec["session_id"], uuid,
        "legacy user_id regex must capture the session uuid: {rec}"
    );
}

/// Source 9 end-to-end: X-Grok-Conv-Id carries the session on the responses
/// protocol, and loses to the standard Session-Id header.
#[tokio::test]
async fn grok_conv_id_session_extraction() {
    start_stack(&[]).await;
    let body = r#"{"model":"m","input":"grok-turn"}"#;
    post_raw("/v1/responses", body, &[("x-grok-conv-id", "grok-conv-42")]).await;
    post_raw(
        "/v1/responses",
        body,
        &[
            ("session-id", "std-wins"),
            ("x-grok-conv-id", "grok-conv-43"),
        ],
    )
    .await;

    let recs = records().await;
    let response_recs: Vec<_> = recs
        .iter()
        // Scoped by the sessions this test created (sibling tests in this
        // binary share one gateway's record store).
        .filter(|r| {
            let sid = r["session_id"].as_str().unwrap_or_default();
            r["protocol"] == "openai.responses" && (sid == "grok-conv-42" || sid == "std-wins")
        })
        .collect();
    assert_eq!(
        response_recs.len(),
        2,
        "two responses turns expected: {recs:?}"
    );
    assert!(
        response_recs
            .iter()
            .any(|r| r["session_id"] == "grok-conv-42"),
        "grok header must be extracted: {recs:?}"
    );
    assert!(
        response_recs.iter().any(|r| r["session_id"] == "std-wins"),
        "standard header must outrank grok: {recs:?}"
    );
}
