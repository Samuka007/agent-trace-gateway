// Behavior: post-response idle teardown (peer closing the idle keep-alive
// connection) is observability noise, not a proxy failure — the completed
// turn exports clean; mid-response RSTs still fail.
// [Requirement: 透明转发；Scenario: 空闲拆除分类]
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

const UPSTREAM_PORT: u16 = 38971;
const GW_PORT: u16 = 38970;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Mini upstream on a BLOCKING thread (per-connection thread): the normal
/// path answers a complete JSON response; a request body containing
/// "half-rst" gets headers + half the body, then an abrupt close (EOF
/// before content-length) — the damaging mid-response class.
fn mini_upstream() {
    let listener = TcpListener::bind(format!("127.0.0.1:{UPSTREAM_PORT}")).expect("bind");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            // Read one request head + body (until the closing brace).
            loop {
                match s.read(&mut tmp) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        let t = String::from_utf8_lossy(&buf);
                        if let Some(hdr_end) = t.find("\r\n\r\n") {
                            if let Some(len) = t[..hdr_end].lines().find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.to_string())
                            }) {
                                let want: usize = len.trim().parse().unwrap_or(0);
                                if buf.len() >= hdr_end + 4 + want {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            if text.contains("half-rst") {
                // Headers + half the declared body, then RST (linger 0).
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n";
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&[b'x'; 50]);
                let _ = s.flush();
                drop(s); // abrupt close mid-body: EOF before content-length
            } else {
                let body = r#"{"output":[{"content":[{"type":"output_text","text":"idle-ok"}]}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
    }
}

/// Raw-socket request: speak HTTP/1.1 by hand, read the COMPLETE response
/// (content-length framed), then either linger-0 RST (idle-teardown
/// simulation — the gateway's next-read probe on the idle keep-alive
/// connection hits ConnectionReset) or a clean close.
fn raw_request(body: &str) -> String {
    let mut s = TcpStream::connect(format!("127.0.0.1:{GW_PORT}")).expect("connect");
    let req = format!(
        "POST /responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = s.read(&mut tmp).expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let t = String::from_utf8_lossy(&buf);
        if let Some(hdr_end) = t.find("\r\n\r\n") {
            if let Some(len) = t[..hdr_end].lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|v| v.to_string())
            }) {
                let want: usize = len.trim().parse().unwrap_or(0);
                if buf.len() >= hdr_end + 4 + want {
                    break;
                }
            }
        }
    }
    drop(s);
    String::from_utf8_lossy(&buf).to_string()
}

async fn records() -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{GW_PORT}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("json")
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

async fn poll_record(user_input: &str) -> serde_json::Value {
    for _ in 0..60 {
        for r in records().await {
            if r["user_input"] == user_input {
                return r;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("record {user_input} never appeared");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_teardown_classification() {
    std::thread::spawn(mini_upstream);
    let gw = format!("127.0.0.1:{GW_PORT}");
    let up = format!("127.0.0.1:{UPSTREAM_PORT}");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw, &up));
    wait_port(UPSTREAM_PORT).await;
    wait_port(GW_PORT).await;

    // (1) Complete response delivered, then the client closes the idle
    // connection (plain close/FIN — a true linger-0 RST needs
    // std::net::TcpStream::set_linger, unstable; the RST-context class is
    // pinned at the classifier level with the exact production string).
    // The turn must export CLEAN (no proxy_error marker) — nothing about
    // the request or response was damaged.
    let resp =
        tokio::task::spawn_blocking(move || raw_request(r#"{"model":"m","input":"idle-clean"}"#))
            .await
            .unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("idle-ok"), "response fully delivered: {resp}");
    let rec = poll_record("idle-clean").await;
    assert!(
        rec.get("error").map(|e| e.is_null()).unwrap_or(true),
        "idle teardown after a delivered response must not mark the turn: {rec:?}"
    );
    assert_eq!(rec["final_output"], "idle-ok", "turn completed: {rec:?}");

    // (2) Negative control — upstream RST mid-body: the response is
    // damaged, the turn MUST fail (existing semantics preserved).
    let req = Request::post(format!("http://127.0.0.1:{GW_PORT}/responses"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","input":"half-rst"}"#.to_string(),
        )))
        .unwrap();
    if let Ok(resp) = client().request(req).await {
        let _ = resp.collect().await;
    }
    let rec = poll_record("half-rst").await;
    assert!(
        rec.get("error").map(|e| !e.is_null()).unwrap_or(false),
        "mid-response RST must still mark the turn errored: {rec:?}"
    );
}

/// Classifier unit pins — the three ruling classes at the discriminator
/// level (fabricating pingora Errors is impractical; the classifier's
/// inputs are exactly these discriminants).
#[test]
fn classifier_three_classes() {
    use agent_trace_gateway::gateway_app::__classify_session_teardown;
    // ① idle RST after a delivered response — either discriminator:
    //    (a) fail-safe belt: the "during HTTP idle state" context (the
    //        string itself encodes post-response; overlapping with (b)).
    assert!(__classify_session_teardown(
        "OS error 104: Connection reset by peer, context: during HTTP idle state",
        true,
        200,
        true
    ));
    assert!(__classify_session_teardown(
        "OS error 104: Connection reset by peer, context: during HTTP idle state",
        false,
        0,
        false
    ));
    //    (b) structural gate: 2xx + end_of_stream reached + downstream
    //        read/close error (no context string).
    assert!(__classify_session_teardown("OS error 104", true, 200, true));
    // ② mid-response damage — never classified:
    //    upstream-sourced read error (downstream_read_or_close=false).
    assert!(!__classify_session_teardown(
        "OS error 104: reset",
        false,
        200,
        true
    ));
    //    BLOCK pin: mid-body downstream disconnect — 2xx HEADERS arrived
    //    but end_of_stream never did: a downstream read/close error here
    //    must NOT classify as idle (a truncated response would launder
    //    into a clean turn).
    assert!(!__classify_session_teardown(
        "OS error 104",
        true,
        200,
        false
    ));
    //    downstream WRITE error mid-relay is not a read/close candidate
    //    (the caller only passes true for Read/ConnectionClosed).
    // ③ no response delivered (dead-connection race) — the conservative
    //    (b) branch requires 2xx AND completion; keeps the existing
    //    failure classification.
    assert!(!__classify_session_teardown("OS error 104", true, 0, true));
    assert!(!__classify_session_teardown(
        "OS error 104",
        true,
        502,
        true
    ));
}
