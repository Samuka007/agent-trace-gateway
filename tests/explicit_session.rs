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
    start_stack().await;
    let gw = common::stack::gateway_port();

    // Real claude-cli samples: three turns of session 01a01f21-eae3-7000-9857-78f64c4de4cc,
    // session id inside metadata.user_id JSON envelope (no header on these).
    let fixture_dir = format!("{}/xtask/harness/fixtures", manifest_dir());
    for name in ["claude_cli_request.json"] {
        let body =
            std::fs::read_to_string(format!("{fixture_dir}/anthropic_messages/{name}")).unwrap();
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
        std::fs::read_to_string(format!("{fixture_dir}/openai_responses/codex_turn1.json"))
            .unwrap();
    post_with_session("/v1/responses", &codex_body, None).await;

    let recs = records().await;

    // claude-cli envelope sessions + header session must share one session id.
    let claude_recs: Vec<_> = recs
        .iter()
        .filter(|r| r["protocol"] == "anthropic_messages")
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
        .find(|r| r["protocol"] == "openai_responses")
        .unwrap_or_else(|| panic!("codex record missing: {recs:?}"));
    assert_eq!(
        codex_rec["session_id"], "01a01f1f-bcff-7c80-94a1-9bbbc9fe9145",
        "codex client_metadata.session_id must be extracted: {codex_rec}"
    );
}
