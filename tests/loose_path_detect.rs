// Behavior: loose (endpoint-variant) path detection — base URLs without
// /v1 still produce traces; loose hits are upstream-gated (2xx only) so a
// 404 on an unknown route mints no fake turn; exact matches keep their
// record-everything semantics.
// [Requirement: 协议解包；Scenario: 宽松路径匹配与上游门控]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder;

const UPSTREAM_PORT: u16 = 37971;
const GW_PORT: u16 = 37970;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Minimal upstream: same-path answers whose status is decided by a body
/// marker ("loose404" -> 404), so one gateway instance exercises both the
/// recorded and the gated loose paths.
async fn mini_upstream() {
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{UPSTREAM_PORT}"))
        .await
        .unwrap();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                let body = req.collect().await.unwrap().to_bytes();
                let text = String::from_utf8_lossy(&body).to_string();
                // Both 404 probes carry an explicit marker; anything
                // else answers 200 (loose-200 and exact-404 differ only
                // in PATH, so the marker must key the status).
                let is_404 = text.contains("loose404") || text.contains("exact404");
                let mut resp = if is_404 {
                    hyper::Response::new(Full::new(Bytes::from("no route")))
                } else {
                    hyper::Response::new(Full::new(Bytes::from(
                        r#"{"output":[{"content":[{"type":"output_text","text":"loose-ok"}]}]}"#,
                    )))
                };
                *resp.status_mut() = if is_404 {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::OK
                };
                resp.headers_mut().insert(
                    hyper::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                Ok::<_, std::convert::Infallible>(resp)
            });
            let _ = Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

async fn post(path: &str, body: &str) -> StatusCode {
    let req = Request::post(format!("http://127.0.0.1:{GW_PORT}{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    let status = resp.status();
    let _ = resp.collect().await;
    status
}

async fn records() -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{GW_PORT}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json")
}

async fn health() -> serde_json::Value {
    let req = Request::get(format!("http://127.0.0.1:{GW_PORT}/__atg/health"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("health");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json")
}

async fn wait_port(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("port {port} never opened");
}

#[tokio::test]
async fn loose_path_detect_and_upstream_gating() {
    tokio::spawn(mini_upstream());
    let gw = format!("127.0.0.1:{GW_PORT}");
    let up = format!("127.0.0.1:{UPSTREAM_PORT}");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw, &up));
    wait_port(UPSTREAM_PORT).await;
    wait_port(GW_PORT).await;

    // 1) Loose hit + 2xx: a base URL without /v1 still traces.
    assert_eq!(
        post("/responses", r#"{"model":"m","input":"loose200"}"#).await,
        StatusCode::OK
    );
    // 2) Loose hit + 404: gated — no fake turn.
    assert_eq!(
        post("/responses", r#"{"model":"m","input":"loose404"}"#).await,
        StatusCode::NOT_FOUND
    );
    // 3) Exact hit + 404: regression — still records (error turn).
    assert_eq!(
        post("/v1/responses", r#"{"model":"m","input":"exact404marker"}"#).await,
        StatusCode::NOT_FOUND
    );
    // 4) Loose chat/completions variant + 2xx: records.
    assert_eq!(
        post(
            "/chat/completions",
            r#"{"model":"m","messages":[{"role":"user","content":"loose-chat"}]}"#
        )
        .await,
        StatusCode::OK
    );

    // Records: logging is async — poll.
    let mut recs = Vec::new();
    for _ in 0..20 {
        recs = records().await;
        if recs.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let loose200 = recs
        .iter()
        .find(|r| r["user_input"] == "loose200")
        .unwrap_or_else(|| panic!("loose 200 record missing: {recs:?}"));
    assert_eq!(
        loose200["protocol"], "openai.responses",
        "loose endpoint variant attributes the protocol: {loose200:?}"
    );
    assert!(
        !recs.iter().any(|r| r["user_input"] == "loose404"),
        "gated loose 404 must not mint a turn: {recs:?}"
    );
    let exact404 = recs
        .iter()
        .find(|r| r["user_input"] == "exact404marker")
        .unwrap_or_else(|| panic!("exact 404 record missing: {recs:?}"));
    assert_eq!(
        exact404["error"], "http_status: 404",
        "exact hits record error turns as before: {exact404:?}"
    );
    let loose_chat = recs
        .iter()
        .find(|r| r["user_input"] == "loose-chat")
        .unwrap_or_else(|| panic!("loose chat record missing: {recs:?}"));
    assert_eq!(loose_chat["protocol"], "openai.chat_completions");

    // Health: three loose hits (requests 1, 2 and 4 — the exact one
    // excluded), regardless of the record outcome.
    let h = health().await;
    assert_eq!(h["loose_path_matches"], 3, "health counters: {h:?}");
}
