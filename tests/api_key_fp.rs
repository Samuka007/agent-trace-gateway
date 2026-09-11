// Behavior: API credentials are fingerprinted (salted sha256, 16 hex) onto
// the trace metadata — same key => same fingerprint across protocols;
// plaintext never appears on any surface; missing credentials are silent.
// [Requirement: 观测面；Scenario: api key 指纹]
mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn post(path: &str, headers: &[(&str, &str)], body: &str) -> StatusCode {
    let gw = common::stack::gateway_port();
    let mut req = Request::post(format!("http://127.0.0.1:{gw}{path}"))
        .header("content-type", "application/json");
    for &(k, v) in headers {
        req = req.header(k, v);
    }
    let resp = client()
        .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
        .await
        .expect("request");
    let status = resp.status();
    let _ = resp.collect().await;
    status
}

async fn poll_record(user_input: &str) -> serde_json::Value {
    let gw = common::stack::gateway_port();
    for _ in 0..60 {
        let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        let resp = client().request(req).await.expect("records");
        let recs: Vec<serde_json::Value> =
            serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json");
        for r in recs {
            if r["user_input"] == user_input {
                return r;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("record {user_input} never appeared");
}

#[tokio::test]
async fn api_key_fingerprint_wire() {
    common::stack::start_stack(&[]).await;

    // Same key, both protocol entry shapes -> SAME fingerprint.
    let auth = ("authorization", "Bearer sk-shared-1");
    post(
        "/v1/chat",
        &[auth, ("x-claude-code-session-id", "fp-sess-1")],
        r#"{"model":"m","messages":[{"role":"user","content":"fp-openai"}]}"#,
    )
    .await;
    let xapi = ("x-api-key", "sk-shared-1");
    post(
        "/v1/messages",
        &[xapi, ("x-claude-code-session-id", "fp-sess-2")],
        r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"fp-anthropic"}]}"#,
    )
    .await;
    // Different key -> DIFFERENT fingerprint.
    post(
        "/v1/chat",
        &[("authorization", "Bearer sk-other-2")],
        r#"{"model":"m","messages":[{"role":"user","content":"fp-other"}]}"#,
    )
    .await;
    // No credential at all -> field absent, no error.
    post(
        "/v1/chat",
        &[],
        r#"{"model":"m","messages":[{"role":"user","content":"fp-none"}]}"#,
    )
    .await;

    let a = poll_record("fp-openai").await;
    let b = poll_record("fp-anthropic").await;
    let c = poll_record("fp-other").await;
    let d = poll_record("fp-none").await;

    let fp = |r: &serde_json::Value| r["api_key_fp"].as_str().unwrap_or_default().to_string();
    assert_eq!(fp(&a).len(), 16, "16 hex chars: {a:?}");
    assert_eq!(
        fp(&a),
        fp(&b),
        "same key => same fingerprint across protocols"
    );
    assert_ne!(fp(&a), fp(&c), "different key => different fingerprint");
    assert!(fp(&d).is_empty(), "no credential => no field: {d:?}");

    // CLI recomputation guarantee: the `gateway key-fp` subcommand calls
    // the same public function the runtime uses — with the default salt
    // (env cleared), its output equals the record's fingerprint exactly.
    std::env::remove_var("ATG_APIKEY_SALT");
    let cli_style = agent_trace_gateway::gateway_app::api_key_fp("sk-shared-1");
    assert_eq!(
        cli_style,
        fp(&a),
        "CLI (same code path) must reproduce the trace metadata value"
    );

    // Plaintext never on any surface: records JSON must not contain the key.
    let gw = common::stack::gateway_port();
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    let raw = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    assert!(
        !raw.contains("sk-shared-1"),
        "plaintext key leaked via records"
    );
    assert!(
        !raw.contains("sk-other-2"),
        "plaintext key leaked via records"
    );
}

/// Salt semantics + recomputation pin: the fingerprint is
/// sha256(salt || key) UTF-8 bytes, first 16 hex chars — the CLI subcommand
/// `gateway key-fp <key>` calls the SAME function, so its output equals
/// every trace's client_key_fp by construction (asserted end-to-end in
/// api_key_fp_wire for the runtime side).
#[test]
fn salt_shape_and_rotation() {
    // Recomputable formula (documented): sha256("salt-v1" || "sk-x") first 16 hex.
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"salt-v1");
    h.update(b"sk-x");
    let manual = hex::encode(h.finalize())[..16].to_string();
    let a = api_key_fp_with_salt("sk-x", "salt-v1");
    assert_eq!(a, manual, "documented formula must hold");
    let b = api_key_fp_with_salt("sk-x", "salt-v2");
    let c = api_key_fp_with_salt("sk-x", "salt-v1");
    assert_eq!(a.len(), 16);
    assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    assert_ne!(a, b, "salt rotation changes the fingerprint");
    assert_eq!(a, c, "deterministic for the same salt+key");
    assert_ne!(
        a,
        api_key_fp_with_salt("sk-y", "salt-v1"),
        "different key => different fingerprint"
    );
}

/// Salt-injected variant of the PUBLIC fingerprint function (the runtime
/// reads the env; this pins the formula under a controlled salt).
fn api_key_fp_with_salt(key: &str, salt: &str) -> String {
    std::env::set_var("ATG_APIKEY_SALT", salt);
    let fp = agent_trace_gateway::gateway_app::api_key_fp(key);
    std::env::remove_var("ATG_APIKEY_SALT");
    fp
}
