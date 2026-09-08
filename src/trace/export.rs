//! OTLP/HTTP export of turn records (JSON encoding) to the configured
//! endpoint. Fail-open by design: bounded queue, drop on overflow or endpoint
//! failure, health counters observable — business traffic is never blocked.
use crate::trace::store::TurnRecord;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const QUEUE_CAPACITY: usize = 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Langfuse vocabulary constants, aligned with sub2api modeltrace
/// (backend/internal/modeltrace/middleware.go): same attribute keys and the
/// line tag let Langfuse-side queries aggregate both producers identically.
const LANGFUSE_TRACE_NAME: &str = "agent.turn";
const LANGFUSE_TRACE_TAG: &str = "line:atg";

#[derive(Default)]
pub struct ExportHealth {
    pub exported: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub dropped: std::sync::atomic::AtomicU64,
}

pub struct Exporter {
    tx: Option<mpsc::Sender<TurnRecord>>,
    pub health: Arc<ExportHealth>,
}

impl Exporter {
    /// Create an exporter toward `endpoint` (e.g.
    /// http://host/api/public/otel). Returns a disabled exporter (tx=None)
    /// when no endpoint is configured or no tokio runtime is available.
    ///
    /// Basic auth may be embedded in the URL userinfo:
    /// `http://user:pass@host/path` → `Authorization: Basic <b64(user:pass)>`
    /// is added to every request and the userinfo stripped from the URL.
    pub fn start(endpoint: Option<String>) -> Self {
        let health = Arc::new(ExportHealth::default());
        let Some(endpoint) = endpoint.filter(|s| !s.trim().is_empty()) else {
            return Self { tx: None, health };
        };
        let (endpoint, auth_header) = split_basic_auth(&endpoint);
        let (tx, rx) = mpsc::channel::<TurnRecord>(QUEUE_CAPACITY);
        let health2 = health.clone();
        // The gateway proxy runs on pingora's threads (no ambient tokio
        // runtime), so the exporter owns a dedicated current-thread runtime.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("export runtime");
            rt.block_on(export_loop(endpoint, auth_header, rx, health2));
        });
        Self {
            tx: Some(tx),
            health,
        }
    }

    /// Queue one record for export. Never blocks; drops (counted) when the
    /// queue is full.
    pub fn submit(&self, record: &TurnRecord) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(record.clone()) {
            Ok(()) => {}
            Err(_) => {
                self.health
                    .dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

async fn export_loop(
    endpoint: String,
    auth_header: Option<String>,
    mut rx: mpsc::Receiver<TurnRecord>,
    health: Arc<ExportHealth>,
) {
    let client = reqwest_client();
    let mut buf: Vec<TurnRecord> = Vec::new();
    let mut last_flush = Instant::now();
    loop {
        match tokio::time::timeout(BATCH_INTERVAL, rx.recv()).await {
            Ok(Some(rec)) => buf.push(rec),
            Ok(None) => break, // channel closed
            Err(_) => {}       // tick: flush if anything buffered
        }
        if buf.is_empty() || last_flush.elapsed() < BATCH_INTERVAL && buf.len() < 32 {
            continue;
        }
        let batch = std::mem::take(&mut buf);
        last_flush = Instant::now();
        flush_batch(&client, &endpoint, &auth_header, &batch, &health).await;
    }
    if !buf.is_empty() {
        flush_batch(&client, &endpoint, &auth_header, &buf, &health).await;
    }
}

fn reqwest_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        // The OTLP endpoint is a trusted local/internal target (e.g. a
        // self-hosted Langfuse on 127.0.0.1). Never route it through an
        // ambient HTTP(S)_PROXY, which would 502 against loopback services.
        .no_proxy()
        .build()
        .unwrap_or_default()
}

async fn flush_batch(
    client: &reqwest::Client,
    endpoint: &str,
    auth_header: &Option<String>,
    batch: &[TurnRecord],
    health: &ExportHealth,
) {
    let payload = build_otlp_json(batch);
    let mut req = client
        .post(endpoint)
        .header("content-type", "application/json");
    if let Some(auth) = auth_header {
        req = req.header("authorization", auth);
    }
    let result = req.body(payload).send().await;
    match result {
        Ok(resp) if resp.status().is_success() => {
            health
                .exported
                .fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        _ => {
            health
                .failed
                .fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Minimal OTLP/HTTP JSON: one resourceSpans with a scopeSpans holding one
/// span per turn (session -> turn organization is expressed through the
/// session.id attribute; consumers group by it).
fn build_otlp_json(batch: &[TurnRecord]) -> String {
    let spans: Vec<serde_json::Value> = batch
        .iter()
        .map(|r| {
            // Explicit session only: modeltrace never writes session
            // attributes when no id exists (middleware.go session == ""), so
            // empty-session turns must not invent one on either line.
            let mut attributes = Vec::new();
            if !r.session_id.is_empty() {
                attributes.push(kv("session.id", &r.session_id));
                attributes.push(kv("langfuse.session.id", &r.session_id));
            }
            attributes.extend([
                kv("protocol", &r.protocol),
                kv("langfuse.trace.name", LANGFUSE_TRACE_NAME),
                kv_array("langfuse.trace.tags", &[LANGFUSE_TRACE_TAG]),
                kv("user_input", &r.user_input),
                kv("final_output", &r.final_output),
                kv("raw_request", &r.raw_request),
                kv("raw_response", &r.raw_response),
                kv("breakpoint", if r.breakpoint { "true" } else { "false" }),
            ]);
            if !r.tool_calls.is_empty() {
                let tool_calls_json = serde_json::to_string(&r.tool_calls).unwrap_or_default();
                attributes.push(kv("tool_calls", &tool_calls_json));
            }
            serde_json::json!({
                "traceId": trace_id_for(&r.session_id),
                "spanId": span_id_for(&r.session_id, &r.user_input, &r.raw_request),
                "name": "agent.turn",
                "kind": 3,
                "startTimeUnixNano": r.start_ns.to_string(),
                "endTimeUnixNano": r.end_ns.to_string(),
                "attributes": attributes
            })
        })
        .collect();
    serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [kv("service.name", "agent-trace-gateway")]
            },
            "scopeSpans": [{
                "scope": {"name": "agent-trace-gateway"},
                "spans": spans
            }]
        }]
    })
    .to_string()
}

fn kv(key: &str, value: &str) -> serde_json::Value {
    serde_json::json!({"key": key, "value": {"stringValue": value}})
}

/// OTLP string-array attribute (Langfuse tags carry array semantics; see
/// modeltrace `otlpStringSlice` / `attribute.StringSlice`).
fn kv_array(key: &str, values: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "value": {
            "arrayValue": {
                "values": values
                    .iter()
                    .map(|v| serde_json::json!({"stringValue": v}))
                    .collect::<Vec<_>>()
            }
        }
    })
}

/// Random 16 bytes from the OS entropy pool, hex-encoded (trace-id shape).
fn random_trace_id() -> String {
    hex::encode(&random_bytes(16))
}

/// Random 8 bytes from the OS entropy pool, hex-encoded (span-id shape).
fn random_span_id() -> String {
    hex::encode(&random_bytes(8))
}

fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    // /dev/urandom is the OS CSPRNG; on read failure fall back to a
    // time-seeded fill instead of panicking the export thread.
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(seed.to_be_bytes());
        for (i, b) in buf.iter_mut().enumerate() {
            *b = d[i % 32];
        }
    }
    buf
}

/// Trace id: deterministic per explicit session (all turns of one session
/// share one trace); empty sessions get a fresh random trace per turn so
/// session-less turns never collapse into one shared trace.
fn trace_id_for(session_id: &str) -> String {
    if session_id.is_empty() {
        return random_trace_id();
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"trace:");
    h.update(session_id.as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// Span id: deterministic per (session, turn content) for explicit sessions;
/// random when sessionless (mirrors trace_id_for's per-turn uniqueness).
fn span_id_for(session_id: &str, user_input: &str, raw_request: &str) -> String {
    if session_id.is_empty() {
        return random_span_id();
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"span:");
    h.update(session_id.as_bytes());
    h.update(user_input.as_bytes());
    h.update(raw_request.len().to_be_bytes());
    hex::encode(&h.finalize()[..8])
}

/// Test helper: current health counters.
impl ExportHealth {
    pub fn snapshot(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.exported.load(Relaxed),
            self.failed.load(Relaxed),
            self.dropped.load(Relaxed),
        )
    }
}

#[allow(dead_code)]
fn _unused(_: &Mutex<()>) {}

/// Split `user:pass@` userinfo out of an http(s) URL, returning the clean URL
/// and a ready-to-send `Authorization: Basic ...` header value. Returns the
/// input URL unchanged with `None` when there is no userinfo.
///
/// Only the first `@` (after the scheme) is consumed; credentials are
/// percent-decoded so `%40` in a password does not break parsing.
fn split_basic_auth(endpoint: &str) -> (String, Option<String>) {
    let Some(scheme_end) = endpoint.find("://") else {
        return (endpoint.to_string(), None);
    };
    let rest = &endpoint[scheme_end + 3..];
    // Find the first @ that appears before any '/' or '?' (i.e. in the
    // authority section only).
    let at = match rest.find('@') {
        Some(i) if rest[..i].chars().all(|c| c != '/' && c != '?' && c != '#') => i,
        _ => return (endpoint.to_string(), None),
    };
    let creds = &rest[..at];
    let clean = format!("{}{}", &endpoint[..scheme_end + 3], &rest[at + 1..]);
    let decoded = percent_decode(creds);
    let b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        decoded.as_bytes(),
    );
    (clean, Some(format!("Basic {b64}")))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session_id: &str) -> TurnRecord {
        TurnRecord {
            session_id: session_id.to_string(),
            protocol: "openai.responses".to_string(),
            ..Default::default()
        }
    }

    fn span_attr<'a>(span: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
        span["attributes"]
            .as_array()?
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| &a["value"])
    }

    /// G1: langfuse.* vocabulary present with aligned values; tags is an
    /// OTLP string-array attribute.
    #[test]
    fn span_carries_langfuse_vocabulary() {
        let payload: serde_json::Value =
            serde_json::from_str(&build_otlp_json(&[record("sess-1")])).unwrap();
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let value = |k: &str| {
            span_attr(span, k)
                .and_then(|v| v["stringValue"].as_str())
                .unwrap_or_else(|| panic!("{k} missing: {span}"))
        };
        assert_eq!(value("session.id"), "sess-1");
        assert_eq!(value("langfuse.session.id"), "sess-1");
        assert_eq!(value("langfuse.trace.name"), "agent.turn");
        let tags = span_attr(span, "langfuse.trace.tags")
            .and_then(|v| v["arrayValue"]["values"].as_array())
            .unwrap_or_else(|| panic!("tags must be an OTLP array: {span}"));
        assert_eq!(tags[0]["stringValue"], "line:atg");
    }

    /// G1 (empty-session variant): session attributes are omitted entirely.
    #[test]
    fn empty_session_omits_session_attributes() {
        let payload: serde_json::Value =
            serde_json::from_str(&build_otlp_json(&[record("")])).unwrap();
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(span_attr(span, "session.id").is_none(), "{span}");
        assert!(span_attr(span, "langfuse.session.id").is_none(), "{span}");
        // Vocabulary constants are still present.
        assert_eq!(
            span_attr(span, "langfuse.trace.name").unwrap()["stringValue"],
            "agent.turn"
        );
    }

    /// G2: two session-less turns must not collapse into one trace.
    #[test]
    fn empty_session_gets_unique_trace_ids() {
        let t1 = trace_id_for("");
        let t2 = trace_id_for("");
        assert_ne!(t1, t2, "session-less turns share a trace id");
        assert_eq!(t1.len(), 32);
        let s1 = span_id_for("", "same input", "same raw");
        let s2 = span_id_for("", "same input", "same raw");
        assert_ne!(s1, s2, "session-less spans share a span id");
        assert_eq!(s1.len(), 16);
    }

    /// Explicit sessions keep the deterministic id scheme (same session ->
    /// same trace id, spanning turns).
    #[test]
    fn explicit_session_keeps_deterministic_ids() {
        assert_eq!(trace_id_for("abc"), trace_id_for("abc"));
        assert_ne!(trace_id_for("abc"), trace_id_for("abd"));
        let payload: serde_json::Value =
            serde_json::from_str(&build_otlp_json(&[record("abc")])).unwrap();
        let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], trace_id_for("abc"));
        assert_eq!(span["spanId"], span_id_for("abc", "", ""));
    }

    #[test]
    fn split_basic_auth_extracts_clean_url_and_header() {
        let (url, auth) =
            split_basic_auth("http://user:pass@127.0.0.1:13000/api/public/otel/v1/traces");
        assert_eq!(url, "http://127.0.0.1:13000/api/public/otel/v1/traces");
        let expect = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                "user:pass".as_bytes()
            )
        );
        assert_eq!(auth.as_deref(), Some(expect.as_str()));
    }

    #[test]
    fn split_basic_auth_ignores_urls_without_userinfo() {
        let (url, auth) = split_basic_auth("http://127.0.0.1:13000/otel");
        assert_eq!(url, "http://127.0.0.1:13000/otel");
        assert!(auth.is_none());
    }

    #[test]
    fn split_basic_auth_does_not_touch_at_sign_in_path() {
        let (url, auth) = split_basic_auth("http://host/nope@x/y");
        assert_eq!(url, "http://host/nope@x/y");
        assert!(auth.is_none());
    }

    #[test]
    fn split_basic_auth_percent_decodes_credentials() {
        let (url, auth) = split_basic_auth("http://user:p%40ss@host/otel");
        assert_eq!(url, "http://host/otel");
        let expect = format!(
            "Basic {}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                "user:p@ss".as_bytes()
            )
        );
        assert_eq!(auth.as_deref(), Some(expect.as_str()));
    }
}
