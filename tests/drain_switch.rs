// Behavior: v0.3.6 drain switch — the per-upstream client-disconnect
// policy. Four nails:
//   (1) drain ON: client drops mid-stream → the upstream stream is still
//       consumed to its natural end → final_output complete, usage
//       complete, cancelled=true, no error marker.
//   (2) drain TIMEOUT: the upstream stalls past ATG_DRAIN_TIMEOUT_SECS →
//       drain abandoned, drain_timed_out=true, partial content, cancelled.
//   (3) drain OFF (default): client drop aborts the upstream connection
//       (the fixture sees the gateway close it) → partial content kept,
//       cancelled=true, no error marker.
//   (4) cancelled turns are never failures: every record above carries
//       no error marker (the fail taxonomy stays v0.3.5).
// [Requirement: 客户端断开处理策略开关；Scenario: drain on/off/timeout + 非 fail 口径]
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const UPSTREAM_PORT: u16 = 39071;
const GW_DRAIN_ON: u16 = 39070;
const GW_DRAIN_TIMEOUT: u16 = 39072;
const GW_DRAIN_OFF: u16 = 39074;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// SSE event helpers (openai.responses wire shape — the descriptor reads
/// text from response.output_text.delta and usage from response.completed).
fn delta(text: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"type": "response.output_text.delta", "delta": text})
    )
}

fn completed(input: u64, output: u64) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": input, "output_tokens": output}}
        })
    )
}

/// Mini upstream: one raw TCP listener, per-connection thread, script
/// selected by the request-body marker. Writes are chunked with sleeps so
/// the client can drop mid-stream deterministically. `saw_upstream_close`
/// is set when the gateway side of the connection closes before the
/// fixture finishes (read EOF) — the direct observable of drain-off
/// upstream abort.
fn mini_upstream(saw_upstream_close: Arc<AtomicBool>) {
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{UPSTREAM_PORT}")).expect("bind");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let saw = saw_upstream_close.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
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
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
            if text.contains("drain-stall") {
                // Two deltas, then hold the connection forever with no
                // more data — the drain window must expire.
                let _ = s.write_all(head.as_bytes());
                for part in ["X", "Y"] {
                    let _ = s.write_all(delta(part).as_bytes());
                    let _ = s.flush();
                    std::thread::sleep(Duration::from_millis(120));
                }
                std::thread::sleep(Duration::from_secs(30));
            } else {
                // drain-full / drain-off: a full stream — 8 deltas then
                // completed usage — while the client may drop midway.
                let _ = s.write_all(head.as_bytes());
                for part in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                    let _ = s.write_all(delta(part).as_bytes());
                    let _ = s.flush();
                    std::thread::sleep(Duration::from_millis(120));
                }
                let _ = s.write_all(completed(11, 22).as_bytes());
                let _ = s.flush();
                // The gateway's abort (drain off) closes this socket even
                // though the fixture never closed it first. Drain on keeps
                // it open until the fixture's own close below.
                let _ = s.set_read_timeout(Some(Duration::from_millis(700)));
                if let Ok(mut probe) = s.try_clone() {
                    match probe.read(&mut tmp) {
                        Ok(0) | Err(_) if !text.contains("drain-full") => {
                            saw.store(true, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
            }
        });
    }
}

/// Send one turn and drop the socket after reading a few stream bytes —
/// the client-cancellation shape.
fn read_partial_then_drop(port: u16, input: &str) -> usize {
    let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    let body = format!(r#"{{"model":"m","stream":true,"input":"{input}"}}"#);
    let req = format!(
        "POST /responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(250))).ok();
    let mut got = 0usize;
    let mut tmp = [0u8; 4096];
    // Stop reading after ~2 events worth of bytes, then hang up.
    while got < 120 {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    drop(s); // client hang-up mid-stream
    got
}

async fn records(port: u16) -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{port}/__atg/records"))
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

async fn poll_record(port: u16, user_input: &str) -> serde_json::Value {
    for _ in 0..80 {
        for r in records(port).await {
            if r["user_input"] == user_input {
                return r;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("record {user_input} never appeared");
}

fn spawn_gateway(port: u16) {
    let gw = format!("127.0.0.1:{port}");
    let up = format!("127.0.0.1:{UPSTREAM_PORT}");
    std::thread::spawn(move || agent_trace_gateway::gateway_app::run(&gw, &up));
}

/// All three drain scenarios run sequentially in one test: the drain
/// config is read from process env at gateway construction, and env is
/// process-global — parallel tests would race on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_switch_scenarios() {
    let saw_upstream_close = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let flag = saw_upstream_close.clone();
        move || mini_upstream(flag)
    });
    wait_port(UPSTREAM_PORT).await;

    // ---- (1) drain ON: complete content + usage after client death ----
    std::env::set_var("ATG_DRAIN_ON_CANCEL", "1");
    spawn_gateway(GW_DRAIN_ON);
    wait_port(GW_DRAIN_ON).await;
    let got = tokio::task::spawn_blocking(|| read_partial_then_drop(GW_DRAIN_ON, "drain-full"))
        .await
        .unwrap();
    assert!(got > 0, "some stream bytes were consumed before the drop");
    let rec = poll_record(GW_DRAIN_ON, "drain-full").await;
    assert_eq!(
        rec["final_output"], "ABCDEFGH",
        "drain must consume the upstream to its natural end: {rec:?}"
    );
    assert_eq!(
        rec["usage"]["input_tokens"], 11,
        "usage must be fully extracted: {rec:?}"
    );
    assert_eq!(rec["usage"]["output_tokens"], 22, "usage complete: {rec:?}");
    assert_eq!(rec["cancelled"], true, "the disconnect is marked: {rec:?}");
    assert!(
        rec.get("error").map(|e| e.is_null()).unwrap_or(true),
        "a cancelled turn is not a failure (nail 4): {rec:?}"
    );

    // ---- (2) drain TIMEOUT: stalled upstream abandoned ----
    std::env::set_var("ATG_DRAIN_TIMEOUT_SECS", "1");
    spawn_gateway(GW_DRAIN_TIMEOUT);
    wait_port(GW_DRAIN_TIMEOUT).await;
    let got =
        tokio::task::spawn_blocking(|| read_partial_then_drop(GW_DRAIN_TIMEOUT, "drain-stall"))
            .await
            .unwrap();
    assert!(got > 0);
    let rec = poll_record(GW_DRAIN_TIMEOUT, "drain-stall").await;
    assert_eq!(
        rec["final_output"], "XY",
        "the partial content captured before the stall is kept: {rec:?}"
    );
    assert_eq!(
        rec["drain_timed_out"], true,
        "the drain window must fire: {rec:?}"
    );
    assert_eq!(rec["cancelled"], true);
    assert!(
        rec.get("error").map(|e| e.is_null()).unwrap_or(true),
        "drain timeout is a cancellation, not a failure: {rec:?}"
    );

    // ---- (3) drain OFF (default): upstream aborted on client drop ----
    std::env::remove_var("ATG_DRAIN_ON_CANCEL");
    std::env::remove_var("ATG_DRAIN_TIMEOUT_SECS");
    spawn_gateway(GW_DRAIN_OFF);
    wait_port(GW_DRAIN_OFF).await;
    let got = tokio::task::spawn_blocking(|| read_partial_then_drop(GW_DRAIN_OFF, "drain-off"))
        .await
        .unwrap();
    assert!(got > 0);
    let rec = poll_record(GW_DRAIN_OFF, "drain-off").await;
    let output = rec["final_output"].as_str().unwrap_or("");
    assert!(
        "ABCDEFGH".starts_with(output) && output.len() < 8,
        "only the delivered partial survives (got {output:?}): {rec:?}"
    );
    assert_eq!(rec["cancelled"], true);
    assert!(
        rec.get("error").map(|e| e.is_null()).unwrap_or(true),
        "cancellation is not an error: {rec:?}"
    );
    // The fixture probes for the gateway-side close only after it finished
    // writing the whole script (~1.8s in) — poll, don't assume.
    assert!(
        (0..50).any(|_| {
            if saw_upstream_close.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
            false
        }),
        "drain off must abort the upstream connection (fixture saw the gateway close it)"
    );
}
