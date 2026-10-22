// Invariant (v0.4.0): every `ATG_*` environment variable is RETIRED — a
// production or bench process started with stale `ATG_*` variables in its
// environment keeps the FILE values: no warning, no failure, silently
// ignored (the positive test of "env is no longer a config surface").
// Replaces the old tests/trace_mode_env_ignored.rs (which only covered
// ATG_TRACE_MODE).
// [Requirement: 配置文件唯一性；Scenario: 遗留 env 被静默忽略]
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

#[tokio::test]
async fn atg_env_vars_are_silently_ignored() {
    // The deployment-misconfiguration shape: stale ATG_* variables set the
    // way an operator's old systemd unit / compose file would.
    std::env::set_var("ATG_CAPTURE_MAX_BYTES", "999");
    std::env::set_var("ATG_TRACE_MODE", "off");
    std::env::set_var("ATG_DRAIN_ON_CANCEL", "1");
    std::env::set_var("ATG_APIKEY_SALT", "env-salt-must-lose");
    std::env::set_var("ATG_OTLP_ENDPOINT", "http://127.0.0.1:1/never");

    // The stack starts from its complete config file — env values must not
    // leak into any effective value.
    start_stack(&[]).await;
    let gw = common::stack::gateway_port();

    // 1. /__atg/config: effective values equal the FILE values.
    let cfg_view = get_json("/__atg/config").await;
    assert_eq!(
        cfg_view["effective"]["capture"]["max_bytes"], 16777216,
        "capture.max_bytes must come from the file, not ATG_CAPTURE_MAX_BYTES: {cfg_view}"
    );
    assert_eq!(
        cfg_view["effective"]["export"]["otlp_endpoint"], "",
        "export.otlp_endpoint must stay the file's empty value, not ATG_OTLP_ENDPOINT: {cfg_view}"
    );
    assert_eq!(
        cfg_view["effective"]["drain"]["on_cancel"], false,
        "drain.on_cancel must come from the file, not ATG_DRAIN_ON_CANCEL: {cfg_view}"
    );
    // The salt never appears anywhere — env or file value.
    let raw = serde_json::to_string(&cfg_view).unwrap();
    assert!(
        !raw.contains("env-salt-must-lose") && !raw.contains(common::cfg::DEFAULT_SALT),
        "neither the env salt nor the file salt may leak through /__atg/config"
    );

    // 2. trace mode: the env said "off" — the bench-only mode must not leak
    //    into a build running the file's "full".
    let h = get_json("/__atg/health").await;
    assert_eq!(h["trace_mode"], "full", "env ATG_TRACE_MODE ignored: {h}");

    // 3. api_key.salt: a real turn's fingerprint must be computed with the
    //    FILE salt (the env salt must lose).
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-env-ignored")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","messages":[{"role":"user","content":"env-ignored-turn"}]}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request served");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();

    let mut fp = String::new();
    for _ in 0..60 {
        let recs = get_json("/__atg/records").await;
        let recs = recs.as_array().cloned().unwrap_or_default();
        if let Some(rec) = recs.iter().find(|r| r["user_input"] == "env-ignored-turn") {
            fp = rec["api_key_fp"].as_str().unwrap_or_default().to_string();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(fp.len(), 16, "record must carry a fingerprint");
    assert_eq!(
        fp,
        agent_trace_gateway::gateway_app::api_key_fp("sk-env-ignored", common::cfg::DEFAULT_SALT),
        "the fingerprint must be the FILE salt's"
    );
    assert_ne!(
        fp,
        agent_trace_gateway::gateway_app::api_key_fp("sk-env-ignored", "env-salt-must-lose"),
        "the env salt must have lost"
    );
}
