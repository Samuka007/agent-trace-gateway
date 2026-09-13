// Behavior: ATG_TRACE_MODE=off — the low-cost tap (v0.3.11). With tracing
// off, the gateway is a near-pure forward: the response body still passes
// through to the client untouched, and the record degrades to a
// timing/protocol shell (no parsing, no raw capture, no error marker).
// [Requirement: 低成本直通档；Scenario: trace off 透明转发 + 壳记录]
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

const UPSTREAM_PORT: u16 = 38991;
const GW_PORT: u16 = 38990;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Minimal upstream: complete JSON responses. Holds the connection after
/// responding so the gateway's upstream read sees a clean stream end only
/// when this task drops it.
async fn mini_upstream() {
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{UPSTREAM_PORT}"))
        .await
        .unwrap();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let body = r#"{"output":[{"content":[{"type":"output_text","text":"off-mode-ok"}]}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let mut s = stream;
            use tokio::io::AsyncWriteExt;
            let _ = s.write_all(resp.as_bytes()).await;
            let _ = s.flush().await;
            // Drop here: connection close is the response end (the response
            // declares connection: close) — the gateway's relay completes.
        });
    }
}

async fn wait_port(port: u16) {
    for _ in 0..300 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("port {port} never opened");
}

async fn records() -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{GW_PORT}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_mode_off_is_transparent_with_shell_records() {
    tokio::spawn(mini_upstream());
    wait_port(UPSTREAM_PORT).await;
    let gw = format!("127.0.0.1:{GW_PORT}");
    let up = format!("127.0.0.1:{UPSTREAM_PORT}");
    std::env::set_var("ATG_TRACE_MODE", "off");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw, &up));
    wait_port(GW_PORT).await;

    // A full turn through the gateway: the response passes through
    // untouched (transparent forward — the fail-open contract).
    let req = Request::post(format!("http://127.0.0.1:{GW_PORT}/v1/responses"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","input":"off-mode"}"#.to_string(),
        )))
        .unwrap();
    let resp = client().request(req).await.expect("request served");
    assert_eq!(resp.status(), 200);
    let body = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();
    assert!(
        body.contains("off-mode-ok"),
        "response must pass through untouched: {body}"
    );

    // The shell record: exists, but carries no parsed content — timing,
    // protocol and the success shape only.
    let mut recs = Vec::new();
    for _ in 0..20 {
        recs = records().await;
        if !recs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let rec = recs
        .iter()
        .find(|r| r["protocol"] == "openai.responses")
        .unwrap_or_else(|| panic!("shell record missing: {recs:?}"));
    assert_eq!(
        rec["final_output"].as_str().unwrap_or(""),
        "",
        "off mode must not parse content: {rec:?}"
    );
    assert!(
        rec.get("error").map(|e| e.is_null()).unwrap_or(true),
        "a successful off-mode turn is not an error: {rec:?}"
    );
    assert!(
        rec["start_ns"].as_u64().unwrap_or(0) > 0,
        "timing observability is kept in off mode: {rec:?}"
    );
}
