// v0.4.0 hot-reload acceptance: the config file can be rewritten IN PLACE
// and `store.reload()` swaps the running values without dropping a single
// in-flight stream. Six nails:
//   (1) streaming zero-interruption: reload DURING a live SSE request —
//       byte-identical body, no 5xx, connection delivered to natural end,
//       generation +1;
//   (2) upstream hot swap: server.upstream A → B applies to NEW requests
//       only after reload (A stops growing, B serves);
//   (3) invalid reload keeps the old config: broken file → Err, old values
//       keep serving, reload_failures +1, generation unchanged;
//   (4) structural keys: changed server.listen lands in restart_required,
//       the RUNNING listen keeps serving, and a simultaneous hot key still
//       applies;
//   (5) salt redaction: /__atg/config reports api_key.salt as "***" and the
//       response never contains the real salt;
//   (6) max_inflight hot change: export.max_inflight 2 → 4 shows up in
//       health without rebuilding the exporter (export_workers constant).
// [Requirement: 配置热重载；Scenario: 流式零中断 / 上游热换 / 非法保旧 / 结构键 / 脱敏 / 热调]
#[path = "common/cfg.rs"]
mod cfg;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

static ATTPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Raw-socket SSE upstream: every connection gets `deltas` output_text
/// events paced `pace` apart plus a completed event with usage — long
/// enough that a reload provably lands mid-request.
fn slow_sse_upstream(port: u16, deltas: usize, pace: Duration) {
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                // Read the request head + body (content-length framed).
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
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                if s.write_all(head.as_bytes()).is_err() {
                    return;
                }
                for i in 0..deltas {
                    let ev = format!(
                        "data: {}\n\n",
                        serde_json::json!({
                            "type": "response.output_text.delta",
                            "delta": format!("reload-delta-{i}")
                        })
                    );
                    if s.write_all(ev.as_bytes()).is_err() {
                        return;
                    }
                    let _ = s.flush();
                    std::thread::sleep(pace);
                }
                let done = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"usage": {"input_tokens": 1, "output_tokens": 2}}
                    })
                );
                let _ = s.write_all(done.as_bytes());
                let _ = s.flush();
            });
        }
    });
}

/// Minimal JSON upstream tagging every response body with its own label and
/// counting connections — the observable of which upstream a request hit.
fn counting_upstream(port: u16, label: &'static str, hits: Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let hits = hits.clone();
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
                hits.fetch_add(1, Ordering::SeqCst);
                let body = format!(
                    r#"{{"output":[{{"content":[{{"type":"output_text","text":"upstream-{label}"}}]}}]}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            });
        }
    });
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

/// Full response (status line + headers + body) of one raw POST, read to
/// natural EOF.
fn raw_post(port: u16, input: &str) -> String {
    // Retry: a just-booted listener under full-suite parallel load can
    // refuse briefly before its accept loop drains.
    let mut s = {
        let addr = format!("127.0.0.1:{port}");
        let mut last;
        loop {
            match std::net::TcpStream::connect(&addr) {
                Ok(s) => break s,
                Err(e) => {
                    last = e.to_string();
                    assert!(
                        ATTPTS.fetch_add(1, Ordering::SeqCst) < 40,
                        "connect {addr} refused for 4s: {last}"
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    };
    let body = format!(r#"{{"model":"m","input":"{input}"}}"#);
    let req = format!(
        "POST /v1/responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 65536];
    loop {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

async fn get_json(port: u16, path: &str) -> serde_json::Value {
    let req = Request::get(format!("http://127.0.0.1:{port}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("endpoint");
    assert_eq!(resp.status(), 200, "{path} must be served");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("JSON body")
}

// ---- (1) streaming zero-interruption ------------------------------------

/// GW 24570, upstream 24571.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_mid_stream_is_zero_interruption() {
    slow_sse_upstream(24571, 12, Duration::from_millis(120));
    let store = cfg::spawn_gateway(
        "reload-stream",
        &cfg::build("127.0.0.1:24570", "127.0.0.1:24571", &[]),
    );
    wait_port(24570).await;

    // Control: the same request with NO reload — the reference bytes.
    let control = tokio::task::spawn_blocking(|| raw_post(24570, "reload-stream"))
        .await
        .unwrap();
    assert!(control.starts_with("HTTP/1.1 200"), "{control}");
    for i in 0..12 {
        assert!(
            control.contains(&format!("reload-delta-{i}")),
            "control stream must carry every delta ({i}): {control}"
        );
    }

    // Live request: start it on its own connection. Its thread records how
    // many bytes had arrived at the FIRST read — the client-side proof the
    // stream was already flowing before we proceed.
    let live = std::thread::spawn(|| {
        let mut s = std::net::TcpStream::connect("127.0.0.1:24570").expect("connect");
        let body = r#"{"model":"m","input":"reload-stream"}"#;
        let req = format!(
            "POST /v1/responses HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 65536];
        let first = s.read(&mut tmp).expect("first read");
        GATEWAY_FIRST_CHUNK.store(first, Ordering::SeqCst);
        buf.extend_from_slice(&tmp[..first]);
        loop {
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    });

    // Wait until the client actually received stream bytes...
    let mut first_bytes = 0usize;
    for _ in 0..500 {
        first_bytes = GATEWAY_FIRST_CHUNK.load(Ordering::SeqCst);
        if first_bytes > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(first_bytes > 0, "stream bytes must flow before the reload");
    // ...and the gateway must report the request as in flight right now.
    let mut inflight = 0u64;
    for _ in 0..250 {
        inflight = get_json(24570, "/__atg/health").await["inflight"]
            .as_u64()
            .unwrap_or(0);
        if inflight >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        inflight >= 1,
        "the live request must be inside the gateway when the reload fires"
    );

    // RELOAD MID-STREAM: strictly between the inflight observation and EOF.
    let before = store.snapshot().generation;
    let report = store.reload().expect("mid-stream reload must succeed");
    assert_eq!(report.generation, before + 1, "generation must advance");

    let body = live.join().expect("live request must complete");
    assert!(body.starts_with("HTTP/1.1 200"), "no 5xx: {body}");
    // Byte-identical bodies (headers legitimately differ: Date), natural
    // end: the reload must not alter a single payload byte.
    let payload = |r: &str| {
        r.split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| r.to_string())
    };
    assert_eq!(
        payload(&body),
        payload(&control),
        "reload must not alter the response body bytes"
    );
    for i in 0..12 {
        assert!(
            body.contains(&format!("reload-delta-{i}")),
            "the live stream must still deliver every delta ({i})"
        );
    }
}

/// Side-channel: bytes observed on the live streaming socket at the FIRST
/// read, set by the streaming test's connection thread before the reload.
static GATEWAY_FIRST_CHUNK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ---- (2) upstream hot swap ----------------------------------------------

/// GW 24574, A 24575, B 24576.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_hot_swap_redirects_new_requests() {
    let a_hits = Arc::new(AtomicUsize::new(0));
    let b_hits = Arc::new(AtomicUsize::new(0));
    counting_upstream(24575, "A", a_hits.clone());
    counting_upstream(24576, "B", b_hits.clone());
    let store = cfg::spawn_gateway(
        "reload-swap",
        &cfg::build("127.0.0.1:24574", "127.0.0.1:24575", &[]),
    );
    wait_port(24574).await;

    let r = raw_post(24574, "swap-1");
    assert!(r.contains("upstream-A"), "first request hits A: {r}");
    assert_eq!(a_hits.load(Ordering::SeqCst), 1);

    // Rewrite server.upstream → B in place and reload.
    let path = store.snapshot().path.clone();
    let new_text = cfg::apply(
        &std::fs::read_to_string(&path).unwrap(),
        "server.upstream",
        &cfg::q("127.0.0.1:24576"),
    );
    cfg::overwrite(&path, &new_text);
    let report = store.reload().expect("swap reload must succeed");
    assert!(
        report.applied.contains(&"server.upstream"),
        "server.upstream is a hot key: {:?}",
        report.applied
    );

    // The NEXT request must hit B; A never grows again.
    let r = raw_post(24574, "swap-2");
    assert!(r.contains("upstream-B"), "post-reload request hits B: {r}");
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        a_hits.load(Ordering::SeqCst),
        1,
        "A must stop receiving traffic after the swap"
    );
}

// ---- (3) invalid reload keeps the old config ----------------------------

/// GW 24578 (upstream address dead by design — no business traffic needed).
#[tokio::test]
async fn invalid_reload_keeps_old_config_serving() {
    let store = cfg::spawn_gateway(
        "reload-invalid",
        &cfg::build("127.0.0.1:24578", "127.0.0.1:24579", &[]),
    );
    wait_port(24578).await;
    let gen_before = store.snapshot().generation;

    // Corrupt the file in place: unknown key + wrong version.
    let path = store.snapshot().path.clone();
    cfg::overwrite(
        &path,
        "version = 99\n[server]\nlisten = \"127.0.0.1:24578\"\nupstream = \"127.0.0.1:24579\"\nnonsense_key = 1\n",
    );
    match store.reload() {
        Err(err) => {
            let _ = err.to_string(); // Display must not panic
        }
        Ok(_) => panic!("corrupt file must fail reload"),
    }

    // The service keeps running on the OLD snapshot: generation unchanged,
    // failure counted, both surfaces still answering.
    assert_eq!(
        store.snapshot().generation,
        gen_before,
        "old config keeps serving"
    );
    assert_eq!(store.reload_failures(), 1, "the failure must be counted");
    let cfg_view = get_json(24578, "/__atg/config").await;
    assert_eq!(
        cfg_view["config"]["generation"], gen_before,
        "the config view must still show the old generation: {cfg_view}"
    );
    assert_eq!(cfg_view["config"]["reload_failures"], 1, "{cfg_view}");
    let h = get_json(24578, "/__atg/health").await;
    assert_eq!(h["config_reload_failures"], 1, "{h}");
}

// ---- (4) structural keys ------------------------------------------------

/// GW 24580 (upstream 24581 dead by design).
#[tokio::test]
async fn structural_change_needs_restart_but_hot_key_applies() {
    let store = cfg::spawn_gateway(
        "reload-structural",
        &cfg::build("127.0.0.1:24580", "127.0.0.1:24581", &[]),
    );
    wait_port(24580).await;
    let running_listen = store.snapshot().cfg.server.listen.clone();

    // Change a structural key (server.listen → an address nothing listens
    // on) AND a hot key (capture.max_bytes) in the same reload.
    let path = store.snapshot().path.clone();
    let text = std::fs::read_to_string(&path).unwrap();
    let text = cfg::apply(&text, "server.listen", &cfg::q("127.0.0.1:39999"));
    let text = cfg::apply(&text, "capture.max_bytes", "4096");
    cfg::overwrite(&path, &text);

    let report = store
        .reload()
        .expect("reload with a structural change still succeeds");
    assert!(
        report.restart_required.contains(&"server.listen"),
        "the structural change must be reported: {:?}",
        report.restart_required
    );
    assert!(
        report.applied.contains(&"capture.max_bytes"),
        "the hot key must still apply in the same reload: {:?}",
        report.applied
    );

    // Surfaces agree: restart_required listed, RUNNING listen unchanged,
    // hot value applied.
    let cfg_view = get_json(24580, "/__atg/config").await;
    let rr = cfg_view["config"]["restart_required"]
        .as_array()
        .expect("restart_required array");
    assert!(
        rr.iter().any(|k| k == "server.listen"),
        "config view restart_required: {cfg_view}"
    );
    assert_eq!(
        cfg_view["structural"]["server.listen"], running_listen,
        "the RUNNING listen must be unchanged (restart needed): {cfg_view}"
    );
    assert_eq!(
        cfg_view["effective"]["capture"]["max_bytes"], 4096,
        "the hot key applied: {cfg_view}"
    );
    let h = get_json(24580, "/__atg/health").await;
    assert_eq!(
        h["config_restart_required"],
        serde_json::json!(["server.listen"]),
        "{h}"
    );
    // ...and the gateway still listens on the OLD port.
    wait_port(24580).await;
}

// ---- (5) salt redaction --------------------------------------------------

/// GW 24584 (upstream 24585 dead by design).
#[tokio::test]
async fn config_view_redacts_the_salt() {
    let store = cfg::spawn_gateway(
        "reload-salt",
        &cfg::build("127.0.0.1:24584", "127.0.0.1:24585", &[]),
    );
    wait_port(24584).await;

    let req = Request::get("http://127.0.0.1:24584/__atg/config")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("config view");
    assert_eq!(resp.status(), 200);
    let raw = String::from_utf8_lossy(&resp.collect().await.unwrap().to_bytes()).to_string();

    let view: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        view["effective"]["api_key"]["salt"], "***",
        "the salt must be redacted: {view}"
    );
    assert!(
        !raw.contains(cfg::DEFAULT_SALT),
        "the REAL salt string must never appear in the response"
    );
    // Redaction is on the view, not the running config:
    assert_eq!(
        store.snapshot().cfg.api_key.salt(),
        cfg::DEFAULT_SALT,
        "the runtime salt itself is intact"
    );
}

// ---- (6) max_inflight hot change ----------------------------------------

/// GW 24588, upstream 24589 (dead by design), export endpoint enabled.
#[tokio::test]
async fn max_inflight_hot_change_without_exporter_rebuild() {
    let store = cfg::spawn_gateway(
        "reload-inflight",
        &cfg::build(
            "127.0.0.1:24588",
            "127.0.0.1:24589",
            &[
                (
                    "export.otlp_endpoint",
                    cfg::q("http://127.0.0.1:24590/api/public/otel"),
                ),
                ("export.max_inflight", "2".to_string()),
                ("export.workers", "2".to_string()),
            ],
        ),
    );
    wait_port(24588).await;

    let h = get_json(24588, "/__atg/health").await;
    assert_eq!(h["export_max_inflight"], 2, "{h}");
    assert_eq!(h["export_workers"], 2, "{h}");
    assert_eq!(h["config_generation"], 1, "{h}");

    // Hot-tune the pool width 2 → 4.
    let path = store.snapshot().path.clone();
    let text = std::fs::read_to_string(&path).unwrap();
    cfg::overwrite(&path, &cfg::apply(&text, "export.max_inflight", "4"));
    store.reload().expect("inflight reload must succeed");

    let h = get_json(24588, "/__atg/health").await;
    assert_eq!(
        h["export_max_inflight"], 4,
        "the new width must be live: {h}"
    );
    assert_eq!(h["config_generation"], 2, "config advanced: {h}");
    assert_eq!(
        h["export_workers"], 2,
        "export_workers is structural — the exporter was NOT rebuilt: {h}"
    );
}
