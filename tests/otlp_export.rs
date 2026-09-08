// Behavior: completed turns are exported as OTLP traces to the configured
// endpoint, organized as session -> turn spans.
// [Requirement: 轨迹导出与故障恢复；Scenario: 轨迹到达审计系统]
mod common;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder;
use parking_lot::Mutex;
use std::sync::Arc;
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
                            let uri = req.uri().to_string();
                            let body = req.collect().await.unwrap().to_bytes();
                            let mut frame = uri.into_bytes();
                            frame.push(b'\n');
                            frame.extend_from_slice(&body);
                            received.lock().push(frame);
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

#[tokio::test]
async fn otlp_export() {
    let collector_port = common::stack::fixture_port() + 100;
    let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    fake_collector(collector_port, received.clone()).await;

    common::stack::start_stack(&[(
        "ATG_OTLP_ENDPOINT",
        format!("http://127.0.0.1:{collector_port}/api/public/otel"),
    )])
    .await;
    let gw = common::stack::gateway_port();

    // One explicit-session turn through the gateway.
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "otlp-turn"}]
    });
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/chat"))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "otlp-session-1")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let resp = client().request(req).await.expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.collect().await.unwrap();

    // Second turn: streaming with a tool call, so the exported span must
    // carry the full tool_calls array.
    let req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
        .header("content-type", "application/json")
        .header("x-codex-turn-metadata", "{\"session_id\":\"otlp-session-1\",\"turn_id\":\"otlp-turn-2\"}")
        .body(Full::new(Bytes::from(
            r#"{"model":"m","stream":true,"input":"otlp-tool-turn","client_metadata":{"session_id":"otlp-session-1"}}"#,
        )))
        .unwrap();
    let resp = client().request(req).await.expect("stream request");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.collect().await.unwrap();

    // Wait for the export flush.
    let mut exported = Vec::new();
    for _ in 0..50 {
        exported = received.lock().clone();
        if !exported.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!exported.is_empty(), "collector received nothing");

    // Parse the OTLP JSON payload: session -> turn span structure.
    let payload: serde_json::Value = serde_json::from_slice(
        &exported[0][exported[0].iter().position(|b| *b == b'\n').unwrap() + 1..],
    )
    .expect("OTLP payload must be JSON");
    // The POST must hit the OTLP HTTP receiver path. The test stack configures
    // a bare host endpoint; the exporter must append /v1/traces itself.
    let uri =
        std::str::from_utf8(&exported[0][..exported[0].iter().position(|b| *b == b'\n').unwrap()])
            .unwrap();
    assert!(
        uri.ends_with("/v1/traces"),
        "OTLP export must target /v1/traces, got {uri}"
    );
    let spans = payload["resourceSpans"]
        .as_array()
        .and_then(|rs| rs.first())
        .and_then(|rs| rs["scopeSpans"].as_array())
        .and_then(|ss| ss.first())
        .and_then(|s| s["spans"].as_array())
        .unwrap_or_else(|| panic!("OTLP spans missing: {payload}"));
    assert_eq!(spans.len(), 2, "two turn spans expected: {spans:?}");
    let span = &spans[0];
    // Session id present as an attribute; turn content carried verbatim.
    let attrs = span["attributes"].as_array().expect("span attributes");
    let attr = |k: &str| {
        attrs
            .iter()
            .find(|a| a["key"] == k)
            .and_then(|a| a["value"]["stringValue"].as_str())
            .map(str::to_string)
            .unwrap_or_default()
    };
    assert_eq!(
        attr("session.id"),
        "otlp-session-1",
        "session attribute: {attrs:?}"
    );
    assert_eq!(attr("protocol"), "openai.chat_completions");
    assert!(
        attr("user_input").contains("otlp-turn"),
        "user input must be exported: {attrs:?}"
    );
    assert!(
        attr("raw_request").contains("otlp-turn"),
        "verbatim request must be exported: {attrs:?}"
    );

    // Timestamps must be real (non-zero nanoseconds), not placeholders.
    for s in spans {
        let start = s["startTimeUnixNano"]
            .as_str()
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        let end = s["endTimeUnixNano"]
            .as_str()
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        assert!(start > 0, "startTimeUnixNano must be a real timestamp: {s}");
        assert!(end >= start, "endTimeUnixNano must be >= start: {s}");
    }

    // The streaming turn must export its tool_calls (name + parseable args).
    let tool_span = spans
        .iter()
        .find(|s| {
            s["attributes"]
                .as_array()
                .map(|a| {
                    a.iter().any(|x| {
                        x["key"] == "protocol" && x["value"]["stringValue"] == "openai.responses"
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("openai.responses span missing: {spans:?}"));
    let tool_attrs = tool_span["attributes"]
        .as_array()
        .expect("tool span attributes");
    let tool_calls_json = tool_attrs
        .iter()
        .find(|a| a["key"] == "tool_calls")
        .and_then(|a| a["value"]["stringValue"].as_str())
        .unwrap_or_else(|| {
            panic!("tool_calls attribute missing on streaming span: {tool_attrs:?}")
        });
    let tool_calls: Vec<serde_json::Value> =
        serde_json::from_str(tool_calls_json).expect("tool_calls must be JSON");
    assert_eq!(
        tool_calls.len(),
        1,
        "one streamed tool call expected: {tool_calls:?}"
    );
    assert_eq!(tool_calls[0]["name"], "read_file");
    let args: serde_json::Value =
        serde_json::from_str(tool_calls[0]["arguments"].as_str().unwrap())
            .expect("exported tool arguments must be complete and parseable");
    assert_eq!(args["path"], "/tmp/x");

    // The record store still holds the record (export does not mutate it).
    let recs = records(gw).await;
    assert_eq!(recs.len(), 2, "both turns must remain in the record store");

    // G1+G2: session-less responses turns (no session id anywhere, no
    // messages array) must not share one trace id and must carry no session
    // attributes — matching modeltrace's "no id, no session attribute".
    for i in 0..2 {
        let body = format!(r#"{{"model":"m","input":"anon-turn-{i}"}}"#);
        let req = Request::post(format!("http://127.0.0.1:{gw}/v1/responses"))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        let resp = client().request(req).await.expect("anon request");
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.collect().await.unwrap();
    }
    // Wait past the batch flush interval so the two anon turns are exported.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let exported_anon = received.lock().clone();
    let anon_spans: Vec<serde_json::Value> = exported_anon
        .iter()
        .filter_map(|p| {
            let split = p.iter().position(|b| *b == b'\n')?;
            serde_json::from_slice::<serde_json::Value>(&p[split + 1..]).ok()
        })
        .flat_map(|payload| {
            payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|s| {
            s["attributes"]
                .as_array()
                .map(|a| {
                    a.iter().any(|x| {
                        x["key"] == "protocol" && x["value"]["stringValue"] == "openai.responses"
                    })
                })
                .unwrap_or(false)
        })
        .collect();
    // The streaming turn from the first scenario is also openai.responses —
    // its span carries a session attribute; the two anon spans must not.
    let anon_only: Vec<_> = anon_spans
        .iter()
        .filter(|s| {
            !s["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "session.id")
        })
        .collect();
    assert_eq!(
        anon_only.len(),
        2,
        "exactly two session-less responses spans expected: {anon_spans:?}"
    );
    let trace_ids: Vec<String> = anon_only
        .iter()
        .map(|s| s["traceId"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_ne!(
        trace_ids[0], trace_ids[1],
        "session-less turns collapsed into one trace"
    );
    for span in &anon_only {
        let has_session_attr = span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "session.id" || a["key"] == "langfuse.session.id");
        assert!(
            !has_session_attr,
            "empty-session span must omit session attributes: {span}"
        );
        assert_eq!(span["name"], "agent.turn");
    }
}
