// Behavior: the gateway keeps EVERY request header it saw (deny list
// excluded) as trace evidence — on the in-process `/__atg/records` snapshot
// and in the OTLP payload — with values capped at 256 chars on a char
// boundary, bounded per turn by an entry cap and a byte budget, plus the
// session provenance marker naming the winning source. Credential carriers
// never appear: the sentinel plaintext is absent from both surfaces.
// [Requirement: 观测面自证；Scenario: header 证据面（黑名单制）]
mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn fake_collector(port: u16, received: Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let received = received.clone();
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let received = received.clone();
                        async move {
                            let body = req.collect().await.unwrap().to_bytes();
                            received.lock().push(body.to_vec());
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(
                                Bytes::from("{}"),
                            )))
                        }
                    },
                );
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
}

async fn records(gw: u16) -> Vec<serde_json::Value> {
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/records"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("records");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("records JSON")
}

async fn health(gw: u16) -> serde_json::Value {
    let req = Request::get(format!("http://127.0.0.1:{gw}/__atg/health"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client().request(req).await.expect("health");
    serde_json::from_slice(&resp.collect().await.unwrap().to_bytes()).expect("health JSON")
}

/// The record whose user text is `needle` (tests share one stack).
fn record_with<'a>(recs: &'a [serde_json::Value], needle: &str) -> &'a serde_json::Value {
    recs.iter()
        .find(|r| r["user_input"].as_str().is_some_and(|s| s.contains(needle)))
        .unwrap_or_else(|| panic!("no record carrying {needle}: {recs:?}"))
}

fn evidence(record: &serde_json::Value) -> Vec<(String, String)> {
    record["hdr_evidence"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let pair = row.as_array().expect("(name, value) pair");
                    (
                        pair[0].as_str().unwrap_or_default().to_string(),
                        pair[1].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Span attributes of the exported payload that belong to the turn whose
/// `user_input` is `needle`.
fn span_attrs_for(payload: &serde_json::Value, needle: &str) -> Vec<(String, String)> {
    let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans");
    let span = spans
        .iter()
        .find(|s| {
            s["attributes"]
                .as_array()
                .map(|a| {
                    a.iter().any(|x| {
                        x["key"] == "user_input"
                            && x["value"]["stringValue"]
                                .as_str()
                                .is_some_and(|v| v.contains(needle))
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no span carrying {needle}: {spans:?}"));
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["key"].as_str().unwrap_or_default().to_string(),
                a["value"]["stringValue"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

const CREDENTIAL_SENTINEL: &str = "SENTINEL-CREDENTIAL";
const DENIED_NAMES: &[&str] = &[
    "authorization",
    "x-api-key",
    "cookie",
    "x-auth-token",
    "x-request-secret",
];

#[tokio::test]
async fn header_evidence_wire() {
    let collector_port = common::stack::fixture_port() + 300;
    let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    fake_collector(collector_port, received.clone()).await;

    common::stack::start_stack(&[(
        "export.otlp_endpoint",
        common::cfg::q(&format!(
            "http://127.0.0.1:{collector_port}/api/public/otel"
        )),
    )])
    .await;
    let gw = common::stack::gateway_port();

    // --- Phase 1: keep everything benign, drop every credential carrier ----
    // Precedence on openai.responses is [session-id, session_id,
    // x-claude-code-session-id, x-grok-conv-id], so the CC header wins and
    // the provenance marker must name it.
    let body = serde_json::json!({"model": "m", "input": "evidence-turn-1"});
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
        .header("content-type", "application/json")
        .header("user-agent", "omp/18.1.0")
        .header("x-grok-conv-id", "grok-conv-1")
        .header("x-claude-code-session-id", "cc-session-1")
        .header("x-opencode-session", "oc-session-1")
        // Headers outside every historical allowlist must survive: the rule
        // is "all except the blacklist".
        .header("x-custom-evidence", "kept-1")
        .header("anthropic-version", "2023-06-01")
        // Credential carriers — explicit entries and marker hits alike.
        .header("authorization", format!("Bearer {}", CREDENTIAL_SENTINEL))
        .header("x-api-key", CREDENTIAL_SENTINEL)
        .header("cookie", format!("sid={}", CREDENTIAL_SENTINEL))
        .header("x-auth-token", CREDENTIAL_SENTINEL)
        .header("x-request-secret", CREDENTIAL_SENTINEL)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("phase1 request");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.collect().await.unwrap();

    let recs = records(gw).await;
    let rec = record_with(&recs, "evidence-turn-1");
    assert_eq!(rec["session_id"], "cc-session-1", "{rec}");
    assert_eq!(
        rec["session_source"], "header:x-claude-code-session-id",
        "session provenance names the winning header: {rec}"
    );
    let ev = evidence(rec);
    let value_of = |name: &str| ev.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    for (name, expected) in [
        ("x-grok-conv-id", "grok-conv-1"),
        ("x-claude-code-session-id", "cc-session-1"),
        ("x-opencode-session", "oc-session-1"),
        ("x-custom-evidence", "kept-1"),
        ("anthropic-version", "2023-06-01"),
    ] {
        assert_eq!(value_of(name), Some(expected), "{name} missing from {ev:?}");
    }
    for denied in DENIED_NAMES {
        assert!(
            value_of(denied).is_none(),
            "denied header {denied} leaked into the evidence: {ev:?}"
        );
    }
    assert!(
        !serde_json::to_string(&recs)
            .unwrap()
            .contains(CREDENTIAL_SENTINEL),
        "credential plaintext must never reach the record surface"
    );

    // --- Phase 2: 257-char values truncate to 256 ---------------------------
    // Header values are visible ASCII by construction (`HeaderValue::to_str`),
    // so the multibyte char-boundary property is nailed by the unit test on
    // `truncate_header_value`; here the wire proves the 256-char cap.
    let long_ascii = "b".repeat(257);
    let long_second = "c".repeat(257);
    let body = serde_json::json!({"model": "m", "input": "evidence-turn-2"});
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
        .header("content-type", "application/json")
        .header("user-agent", format!("{}{}", "u".repeat(300), "tail"))
        .header("x-grok-conv-id", long_ascii)
        .header("x-claude-code-session-id", long_second)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("phase2 request");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.collect().await.unwrap();

    let recs = records(gw).await;
    let rec = record_with(&recs, "evidence-turn-2");
    let ua = rec["client_ua"].as_str().expect("client_ua");
    assert_eq!(ua.chars().count(), 256, "UA caps at 256 chars");
    assert_eq!(ua, "u".repeat(256));
    let ev = evidence(rec);
    let by_name = |name: &str| {
        ev.iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} missing from {ev:?}"))
            .1
            .clone()
    };
    assert_eq!(by_name("x-grok-conv-id"), "b".repeat(256));
    assert_eq!(by_name("x-claude-code-session-id"), "c".repeat(256));

    // --- Phase 3: the per-turn bounds are deterministic and counted ---------
    // 40 custom headers with 300-char values: values truncate to 256, then
    // the entry cap / byte budget drops the overflow. Standard request
    // headers share the budget, so the exact split is not pinned — the
    // invariants are (count, total bytes, drops counted).
    let filler = "z".repeat(300);
    let body = serde_json::json!({"model": "m", "input": "evidence-turn-3"});
    let mut req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
        .header("content-type", "application/json");
    for i in 0..40 {
        req = req.header(format!("x-ev-{i:02}"), filler.clone());
    }
    let resp = client()
        .request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
        .await
        .expect("phase3 request");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.collect().await.unwrap();

    let recs = records(gw).await;
    let rec = record_with(&recs, "evidence-turn-3");
    let ev = evidence(rec);
    assert!(ev.len() <= 32, "entry cap: {} entries in {ev:?}", ev.len());
    let bytes: usize = ev.iter().map(|(n, v)| n.len() + v.len()).sum();
    assert!(bytes <= 4096, "byte budget: {bytes} in {ev:?}");
    let kept_custom = ev.iter().filter(|(n, _)| n.starts_with("x-ev-")).count();
    assert!(
        (5..40).contains(&kept_custom),
        "custom headers partially kept (budget applies): {kept_custom} of 40"
    );
    assert!(
        ev.iter().all(|(_, v)| v.chars().count() <= 256),
        "every kept value is capped: {ev:?}"
    );
    let h = health(gw).await;
    assert!(
        h["hdr_evidence_dropped"].as_u64().unwrap_or(0) >= 20,
        "bound drops are counted in health: {h}"
    );

    // --- Export surface: the same evidence rides the OTLP span -------------
    // Batcher window is 500 ms; poll generously.
    let mut payload: Option<serde_json::Value> = None;
    for _ in 0..60 {
        let frames: Vec<serde_json::Value> = received
            .lock()
            .iter()
            .filter_map(|body| serde_json::from_slice(body).ok())
            .collect();
        if let Some(found) = frames.into_iter().find(|p| {
            p["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .map(|spans| {
                    spans.iter().any(|s| {
                        s["attributes"].as_array().is_some_and(|a| {
                            a.iter().any(|x| {
                                x["key"] == "user_input"
                                    && x["value"]["stringValue"]
                                        .as_str()
                                        .is_some_and(|v| v.contains("evidence-turn-1"))
                            })
                        })
                    })
                })
                .unwrap_or(false)
        }) {
            payload = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let payload = payload.expect("exported payload for evidence-turn-1");
    let attrs = span_attrs_for(&payload, "evidence-turn-1");
    let attr = |key: &str| {
        attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        attr("langfuse.trace.metadata.session_source"),
        Some("header:x-claude-code-session-id"),
        "span carries the session provenance: {attrs:?}"
    );
    assert_eq!(
        attr("langfuse.trace.metadata.hdr.x-custom-evidence"),
        Some("kept-1"),
        "a header outside every historical allowlist rides the span: {attrs:?}"
    );
    assert_eq!(
        attr("langfuse.trace.metadata.hdr.anthropic-version"),
        Some("2023-06-01"),
        "{attrs:?}"
    );
    for denied in DENIED_NAMES {
        let key = format!("langfuse.trace.metadata.hdr.{denied}");
        assert!(
            attr(&key).is_none(),
            "denied header {denied} became an attribute: {attrs:?}"
        );
    }
    let joined = received.lock().concat();
    let text = String::from_utf8_lossy(&joined);
    assert!(
        !text.contains(CREDENTIAL_SENTINEL),
        "credential plaintext never reaches the wire"
    );
}
