// Behavior: after a gateway restart, prefix-stitched sessions start a new
// trajectory segment (state is in-process), while explicit session ids keep
// stitching — no errors either way.
// [Requirement: 会话串联；Scenario: 网关重启后的串联状态]

// Only the fixture server — this test embeds its own gateway stack (below)
// and must not pull the shared stack helpers (dead_code under clippy).
#[path = "common/fixture_server.rs"]
mod fixture_server;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

const FIXTURE_PORT: u16 = 17961;
const GW1: u16 = 17960;
const GW2: u16 = 17962;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn post(port: u16, body: &str, session_header: Option<&str>) {
    let mut req = Request::post(format!("http://127.0.0.1:{port}/v1/chat"))
        .header("content-type", "application/json");
    if let Some(h) = session_header {
        req = req.header("x-claude-code-session-id", h);
    }
    let resp = client()
        .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let _ = resp.collect().await.unwrap();
}

async fn records(port: u16) -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{port}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("records JSON")
}

#[tokio::test]
async fn restart_stitch_state() {
    // Instance 1.
    tokio::spawn(async move {
        let listen = format!("127.0.0.1:{FIXTURE_PORT}");
        let _ = fixture_server::serve(&listen).await;
    });
    let gw1 = format!("127.0.0.1:{GW1}");
    let up1 = format!("127.0.0.1:{FIXTURE_PORT}");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw1, &up1));
    // Instance 2 ("restart").
    let gw2 = format!("127.0.0.1:{GW2}");
    let up2 = format!("127.0.0.1:{FIXTURE_PORT}");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw2, &up2));
    for port in [FIXTURE_PORT, GW1, GW2] {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    let explicit_body = r#"{"model":"m","messages":[{"role":"user","content":"explicit-turn"}]}"#;
    let prefix_body_a = r#"{"model":"m","messages":[{"role":"system","content":"rs-sys"},{"role":"user","content":"rs-u1"}]}"#;
    let prefix_body_b = r#"{"model":"m","messages":[{"role":"system","content":"rs-sys"},{"role":"user","content":"rs-u1"},{"role":"assistant","content":"ok"},{"role":"user","content":"rs-u2"}]}"#;

    // Before restart: explicit session + one prefix chain.
    post(GW1, explicit_body, Some("restart-sess-1")).await;
    post(GW1, prefix_body_a, None).await;
    post(GW1, prefix_body_b, None).await;
    // After "restart" (instance 2): same explicit id, same prefix history.
    post(GW2, explicit_body, Some("restart-sess-1")).await;
    post(GW2, prefix_body_a, None).await;

    let r1 = records(GW1).await;
    let r2 = records(GW2).await;

    // Explicit id stitches within each instance's records (stateless).
    let exp1: Vec<_> = r1
        .iter()
        .filter(|r| r["session_id"] == "restart-sess-1")
        .collect();
    let exp2: Vec<_> = r2
        .iter()
        .filter(|r| r["session_id"] == "restart-sess-1")
        .collect();
    assert_eq!(exp1.len(), 1, "explicit session on instance 1: {r1:?}");
    assert_eq!(
        exp2.len(),
        1,
        "explicit id must keep stitching after restart: {r2:?}"
    );

    // Prefix chain: both requests on instance 1 share one synthetic session.
    let pfx1: Vec<_> = r1
        .iter()
        .filter(|r| r["session_id"].as_str().unwrap_or("").starts_with("pfx:"))
        .collect();
    assert_eq!(pfx1.len(), 2, "prefix chain on instance 1: {r1:?}");
    assert_eq!(pfx1[0]["session_id"], pfx1[1]["session_id"]);

    // After restart the chain state is gone: the re-sent prefix request opens
    // a fresh synthetic session — and produces no error.
    let pfx2: Vec<_> = r2
        .iter()
        .filter(|r| r["session_id"].as_str().unwrap_or("").starts_with("pfx:"))
        .collect();
    assert_eq!(pfx2.len(), 1, "prefix request on instance 2: {r2:?}");
    assert_ne!(
        pfx2[0]["session_id"], pfx1[0]["session_id"],
        "prefix chain must not silently continue across restart"
    );
}
