//! OTLP/HTTP export of turn records (JSON encoding) to the configured
//! endpoint. Fail-open by design: bounded queue, drop on overflow or endpoint
//! failure, health counters observable — business traffic is never blocked.
// PANIC-AUDIT v0.3.8: audited file — serde_json Value key-index (miss →
// Null, never panics on objects) and provably-bounded slices/arithmetic on
// locally-owned buffers (wire bodies capped by the capture layer). The
// indexing/arithmetic lints are syntax-broad here; tracked in the
// PanicAudit issue.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use atg_model::TurnRecord;
use futures_util::FutureExt;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const QUEUE_CAPACITY: usize = 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(500);

use atg_model::{
    usage_details_json, ATTR_COMPLETION_START_TIME, ATTR_MODEL_NAME, ATTR_OBSERVATION_INPUT,
    ATTR_OBSERVATION_OUTPUT, ATTR_OBSERVATION_TYPE, ATTR_USAGE_DETAILS, ATTR_USER_ID,
    GENERATION_SPAN_NAME, LANGFUSE_TRACE_NAME, LANGFUSE_TRACE_TAG, OBSERVATION_TYPE_GENERATION,
};

#[derive(Default)]
pub struct ExportHealth {
    pub exported: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub dropped: std::sync::atomic::AtomicU64,
    /// Batch flushes aborted by a panic inside the export task
    /// (v0.3.8 seam: the task survives and keeps exporting).
    pub panicked: std::sync::atomic::AtomicU64,
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
            // PANIC-AUDIT v0.3.8: exporter thread startup — a runtime
            // build failure is fatal by design and must abort the thread
            // (class ii, process startup path).
            #[allow(clippy::expect_used)]
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
        // v0.3.8 seam: a panic inside a flush must not kill the export
        // task — the task dying here silently and permanently stops ALL
        // exports while the gateway keeps serving. Degrade: count the
        // batch as failed+panicked and continue with the next batch.
        let flushed = std::panic::AssertUnwindSafe(flush_batch(
            &client,
            &endpoint,
            &auth_header,
            &batch,
            &health,
        ))
        .catch_unwind()
        .await;
        if flushed.is_err() {
            health
                .failed
                .fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed);
            health
                .panicked
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "OTLP export loop: flush panicked — batch dropped ({} records), export continues",
                batch.len()
            );
        }
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
        .post(normalize_endpoint_path(endpoint))
        .header("content-type", "application/json")
        .header(
            atg_model::INGESTION_VERSION_HEADER,
            atg_model::INGESTION_VERSION,
        );
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
            // Fail-open still demands observability: surface why a batch died
            // (connect refused, non-2xx status, body errors) without ever
            // blocking or leaking the traced content.
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let body = body.chars().take(200).collect::<String>();
                    eprintln!(
                        "OTLP export failed: endpoint={} status={status} body={body:?}",
                        display_endpoint(endpoint)
                    );
                }
                Err(e) => {
                    eprintln!(
                        "OTLP export failed: endpoint={} error={e}",
                        display_endpoint(endpoint)
                    );
                }
            }
        }
    }
}

/// Ensure the POST path targets the OTLP HTTP receiver's `/v1/traces`.
/// The otelcol-contrib OTLP/HTTP receiver only accepts POSTs there; users
/// configuring `http://host:4318` (host only) previously exported to `/` and
/// every batch failed. Langfuse's self-hosted OTLP gateway path
/// (`/api/public/otel/v1/traces`) is already complete and is kept verbatim.
fn normalize_endpoint_path(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        return endpoint.to_string();
    }
    format!("{trimmed}/v1/traces")
}

/// Endpoint for log lines with userinfo stripped (credentials never logged).
fn display_endpoint(endpoint: &str) -> String {
    let (clean, _) = split_basic_auth(endpoint);
    clean
}

/// Minimal OTLP/HTTP JSON: one resourceSpans with a scopeSpans holding one
/// span per turn (session -> turn organization is expressed through the
/// session.id attribute; consumers group by it).
fn build_otlp_json(batch: &[TurnRecord]) -> String {
    let spans: Vec<serde_json::Value> = batch
        .iter()
        .flat_map(|r| {
            // Explicit session only: modeltrace never writes session
            // attributes when no id exists (middleware.go session == ""), so
            // empty-session turns must not invent one on either line.
            // Trace-level attributes (session/tags/name) are copied onto
            // every span in the trace (spec section 3).
            let mut attributes = Vec::new();
            // F2 harness wire (trace-level): tag harness:<name> +
            // langfuse.trace.metadata.{harness,…}.
            let harness_tag = (!r.harness.is_empty()).then(|| format!("harness:{}", r.harness));
            let tags: Vec<&str> = match &harness_tag {
                Some(t) => vec![LANGFUSE_TRACE_TAG, t.as_str()],
                None => vec![LANGFUSE_TRACE_TAG],
            };
            let mut trace_extra: Vec<serde_json::Value> = Vec::new();
            if !r.harness.is_empty() {
                trace_extra.push(kv("langfuse.trace.metadata.harness", &r.harness));
            }
            // Two-tier: the session-carrying dialect, independent of the
            // identity (omp borrows the claude-code dialect).
            if !r.dialect.is_empty() {
                trace_extra.push(kv("langfuse.trace.metadata.dialect", &r.dialect));
            }
            // Attribution-evidence audit: the UA the gateway actually saw.
            if !r.client_ua.is_empty() {
                trace_extra.push(kv("langfuse.trace.metadata.client_ua", &r.client_ua));
            }
            // Client cancelled mid-turn (three-way error taxonomy): the
            // turn records normally with partial content; the marker is
            // reconciliation material (upstream may have drained/billed).
            if r.cancelled {
                trace_extra.push(kv("langfuse.trace.metadata.cancelled", "true"));
            }
            // Salted API-credential fingerprint: key-reuse correlation
            // without the key (16-hex salted sha256, plaintext never
            // leaves the gateway).
            if !r.api_key_fp.is_empty() {
                trace_extra.push(kv("langfuse.trace.metadata.client_key_fp", &r.api_key_fp));
            }
            if r.harness_anomaly {
                trace_extra.push(kv(
                    "langfuse.trace.metadata.harness_protocol_anomaly",
                    "true",
                ));
            }
            // §E ruling: synthetic (stitcher-minted) sessions are tagged so
            // Langfuse-side queries can exclude them from hit-rate numerators.
            if r.session_synthetic {
                trace_extra.push(kv("langfuse.trace.metadata.session_synthetic", "true"));
            }
            if r.harness_candidates.len() > 1 {
                let joined = r.harness_candidates.join(",");
                trace_extra.push(kv("langfuse.trace.metadata.harness_candidates", &joined));
            }
            for (k, v) in &r.harness_enrich {
                trace_extra.push(kv(&format!("langfuse.trace.metadata.{k}"), v));
            }
            // P1-10: modeltrace-aligned trace metadata — the entry protocol
            // and the client-declared model (queryable cross-line).
            trace_extra.push(kv("langfuse.trace.metadata.entry_protocol", &r.protocol));
            if !r.model_name.is_empty() {
                trace_extra.push(kv("langfuse.trace.metadata.client_model", &r.model_name));
            }
            if !r.session_id.is_empty() {
                // P2-14: single official key — dual spelling converged.
                attributes.push(kv("langfuse.session.id", &r.session_id));
            }
            attributes.extend([
                kv("protocol", &r.protocol),
                kv("langfuse.trace.name", LANGFUSE_TRACE_NAME),
                kv_array("langfuse.trace.tags", &tags),
                kv(ATTR_OBSERVATION_TYPE, OBSERVATION_TYPE_GENERATION),
                kv("user_input", &r.user_input),
                kv("final_output", &r.final_output),
                kv("raw_request", &r.raw_request),
                kv("raw_response", &r.raw_response),
                kv("breakpoint", if r.breakpoint { "true" } else { "false" }),
            ]);
            // P0-3: official observation content keys (UI panel reads these);
            // empty strings are omitted. LEGAL on generations (the mapping
            // table allows input/output on any observation type).
            if !r.user_input.is_empty() {
                attributes.push(kv(ATTR_OBSERVATION_INPUT, &r.user_input));
            }
            if !r.user_id.is_empty() {
                attributes.push(kv(ATTR_USER_ID, &r.user_id));
            }
            if !r.final_output.is_empty() {
                attributes.push(kv(ATTR_OBSERVATION_OUTPUT, &r.final_output));
            }
            if !r.model_name.is_empty() {
                attributes.push(kv(ATTR_MODEL_NAME, &r.model_name));
            }
            // P1-8: generation-exclusive completion start (ISO 8601 Z,
            // nanosecond precision) — first output byte on the wire
            // (streaming) or the request start (non-streaming).
            if let Some(ns) = r.completion_start_ns {
                attributes.push(kv(ATTR_COMPLETION_START_TIME, &iso8601_z(ns)));
            }
            // Usage (exclusive buckets) — generation-only field, now on the
            // root generation itself.
            if let Some(u) = &r.usage {
                // G1: an all-zero usage passes the is_empty() gate but
                // serializes to "{}" — omit the attribute entirely
                // (zero ≙ unreported, same rule as the entry level).
                let details = usage_details_json(u);
                if details != "{}" {
                    attributes.push(kv(ATTR_USAGE_DETAILS, &details));
                }
            }
            if !r.tool_calls.is_empty() {
                let tool_calls_json = serde_json::to_string(&r.tool_calls).unwrap_or_default();
                attributes.push(kv("tool_calls", &tool_calls_json));
            }
            attributes.extend(trace_extra.iter().cloned());
            // P0-N1 (carried over): one random traceId + spanId per TURN.
            let trace_id = random_trace_id();
            let mut span = serde_json::json!({
                "traceId": trace_id,
                "spanId": random_span_id(),
                "name": GENERATION_SPAN_NAME,
                "kind": 3,
                "startTimeUnixNano": r.start_ns.to_string(),
                "endTimeUnixNano": r.end_ns.to_string(),
                "attributes": attributes
            });
            // AMB-7: native OTLP span status. Langfuse's native-OTLP
            // property mapping derives each observation's `level` from
            // `span.status.code` and its `statusMessage` from
            // `span.status.message` (observation-level mapping table:
            // level ← "Inferred from span.status.code", statusMessage ←
            // "Inferred from span.status.message"). Native form chosen
            // over a `langfuse.observation.status_message` attribute
            // because the native block sets both level and message.
            if let Some(err) = &r.error {
                span["status"] = serde_json::json!({
                    // OTLP StatusCode::Error — Langfuse maps to level=ERROR.
                    "code": 2,
                    "message": err
                });
            }
            vec![span]
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

/// Unix nanoseconds → ISO 8601 UTC string with nanosecond precision
/// (e.g. "2026-09-09T03:12:59.123456789Z"). Civil-from-days per Howard
/// Hinnant's algorithm (std-only; no chrono dependency).
fn iso8601_z(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let nanos = ns % 1_000_000_000;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    // days since 1970-01-01 → (y, m, d) in the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{nanos:09}Z")
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
    hex::encode(random_bytes(16))
}

/// Random 8 bytes from the OS entropy pool, hex-encoded (span-id shape).
fn random_span_id() -> String {
    hex::encode(random_bytes(8))
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

/// Test helper: current health counters.
impl ExportHealth {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.exported.load(Relaxed),
            self.failed.load(Relaxed),
            self.dropped.load(Relaxed),
            self.panicked.load(Relaxed),
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
        // P2-14: single official key (dual spelling converged).
        assert_eq!(value("langfuse.session.id"), "sess-1");
        assert_eq!(value("langfuse.trace.name"), "agent.turn");
        // GENERATION-only shape: one span per turn, the root generation.
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1, "one root generation per turn: {spans:?}");
        assert_eq!(span["name"], "agent.turn.generation");
        assert_eq!(value("langfuse.observation.type"), "generation");
        // Without usage the root generation carries no usage_details.
        assert!(span_attr(span, "langfuse.observation.usage_details").is_none());
        // No container: the root generation has no parentSpanId.
        assert!(span.get("parentSpanId").is_none(), "{span}");
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

    /// The root generation carries usage_details when usage is present.
    #[test]
    fn generation_span_carries_usage_details() {
        let mut r = record("sess-usage");
        r.usage = Some(atg_model::TurnUsage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            cache_read_tokens: Some(3),
            cache_creation_tokens: Some(4),
            total_tokens: None,
        });
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        let generation = &spans[0];
        assert_eq!(generation["name"], "agent.turn.generation");
        let value = |k: &str| {
            span_attr(generation, k)
                .and_then(|v| v["stringValue"].as_str())
                .unwrap_or_else(|| panic!("{k} missing: {generation}"))
        };
        assert_eq!(
            value("langfuse.observation.usage_details"),
            r#"{"cache_creation_input_tokens":4,"cache_read_input_tokens":3,"input":12,"output":7}"#
        );
        // total omitted: derived server-side as bucket sum.
    }

    /// G2/P0-1: ids are random per turn — replaying the same record twice
    /// (same session, same content) must yield distinct trace/span ids so
    /// Langfuse's span upsert never silently swallows turns.
    #[test]
    fn replayed_turns_get_distinct_ids() {
        // 5 identical records -> 5 distinct root-generation ids.
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(record("sess-1"));
        }
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&records)).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let mut ids: Vec<&str> = spans
            .iter()
            .map(|s| s["spanId"].as_str().unwrap())
            .collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "duplicate span ids across turns: {ids:?}"
        );
        // Same request replayed N times -> N distinct trace ids.
        let mut trace_ids: Vec<String> = (0..5).map(|_| random_trace_id()).collect();
        trace_ids.sort();
        let before = trace_ids.len();
        trace_ids.dedup();
        assert_eq!(trace_ids.len(), before, "replayed turns shared a trace id");
    }

    /// P0-N1 (GENERATION-only era): one span per turn — every turn gets a
    /// fresh traceId and spanId (no upsert collisions, no cross-turn trace
    /// sharing); the historical parent/child linkage assertions are void
    /// with the container removed.
    #[test]
    fn turns_get_one_span_and_distinct_traces() {
        let records: Vec<_> = (0..5).map(|_| record("sess-t")).collect();
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&records)).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 5, "one root generation per turn");
        for s in spans {
            assert!(s.get("parentSpanId").is_none(), "no container: {s}");
        }
        let mut trace_ids: Vec<String> = spans
            .iter()
            .map(|s| s["traceId"].as_str().unwrap_or_default().to_string())
            .collect();
        trace_ids.sort();
        let before = trace_ids.len();
        trace_ids.dedup();
        assert_eq!(trace_ids.len(), before, "turns must not share traces");
    }

    /// G1: an all-zero usage is unreported — the usage_details attribute
    /// is omitted entirely, never an empty "{}" object.
    #[test]
    fn all_zero_usage_omits_usage_details() {
        let mut r = record("sess-g1");
        r.usage = Some(atg_model::TurnUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            total_tokens: Some(0),
            ..Default::default()
        });
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        for s in spans {
            assert!(
                span_attr(s, ATTR_USAGE_DETAILS).is_none(),
                "all-zero usage must not emit usage_details: {s}"
            );
        }
    }

    /// AMB-7 (GENERATION-only): an errored turn marks the root generation
    /// — Langfuse infers the observation's level from span.status.code.
    #[test]
    fn errored_turn_marks_the_generation() {
        let mut r = record("sess-err");
        r.error = Some("response.failed: upstream 500".to_string());
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s["status"]["code"], 2, "the generation must be ERROR: {s}");
        assert_eq!(
            s["status"]["message"], "response.failed: upstream 500",
            "statusMessage must carry the error text: {s}"
        );
    }

    /// v0.3.6 (drain switch): a cancelled turn — including one whose drain
    /// window expired — is reconciliation material, NOT a failure: no
    /// OTLP ERROR status (Langfuse level stays non-ERROR), and the
    /// cancelled fact rides trace metadata only.
    #[test]
    fn cancelled_and_drain_timed_out_turns_are_not_errors() {
        let mut cancelled = record("sess-cancel");
        cancelled.cancelled = true;
        let mut drain_to = record("sess-drain-to");
        drain_to.cancelled = true;
        drain_to.drain_timed_out = true;
        for (label, r) in [("cancelled", cancelled), ("drain-timed-out", drain_to)] {
            let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
            let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap();
            let s = &spans[0];
            assert!(
                s.get("status").is_none(),
                "{label}: cancelled turns must not carry an error status: {s}"
            );
            assert_eq!(
                span_attr(s, "langfuse.trace.metadata.cancelled")
                    .and_then(|v| v["stringValue"].as_str()),
                Some("true"),
                "{label}: the cancellation fact must be metadata: {s}"
            );
        }
    }

    /// F2: harness attribution rides trace.tags + trace.metadata on the
    /// root generation; enrich pairs land as
    /// langfuse.trace.metadata.<key>; the borrowed dialect rides its own
    /// metadata.dialect key (omp case).
    #[test]
    fn harness_turn_emits_tag_and_metadata() {
        let mut r = record("sess-h");
        r.harness = "omp".to_string();
        r.dialect = "claude-code".to_string();
        r.harness_candidates = vec!["claude-code".to_string(), "codex".to_string()];
        r.harness_enrich
            .push(("cc_account".to_string(), "acc-1".to_string()));
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        for s in spans {
            let value = |k: &str| {
                span_attr(s, k)
                    .and_then(|v| v["stringValue"].as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            assert_eq!(value("langfuse.trace.metadata.harness"), "omp");
            assert_eq!(
                value("langfuse.trace.metadata.dialect"),
                "claude-code",
                "borrowed dialect is independent of the identity"
            );
            assert_eq!(value("langfuse.trace.metadata.cc_account"), "acc-1");
            assert_eq!(
                value("langfuse.trace.metadata.harness_candidates"),
                "claude-code,codex"
            );
            let tags = span_attr(s, "langfuse.trace.tags")
                .and_then(|v| v["arrayValue"]["values"].as_array())
                .unwrap_or_else(|| panic!("tags missing: {s}"));
            assert!(
                tags.iter().any(|t| t["stringValue"] == "harness:omp"),
                "identity tag on every span: {tags:?}"
            );
        }
    }

    /// P1-8/P1-10: generation span carries the ISO-8601 completion start
    /// (nanosecond precision); trace metadata carries entry_protocol and
    /// client_model on both spans.
    #[test]
    fn completion_start_and_trace_metadata_wire() {
        let mut r = record("sess-p18");
        r.model_name = "m".to_string();
        // 2026-09-09T00:00:00.000000042Z
        r.completion_start_ns = Some(1_788_912_000_000_000_042);
        let payload: serde_json::Value = serde_json::from_str(&build_otlp_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        let generation = &spans[0];
        let value = |k: &str| -> String {
            span_attr(generation, k)
                .and_then(|v| v["stringValue"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(
            value("langfuse.observation.completion_start_time"),
            "2026-09-09T00:00:00.000000042Z",
            "ISO 8601 Z with nanosecond precision"
        );
        // P1-10 trace metadata, modeltrace-aligned key names.
        assert_eq!(
            value("langfuse.trace.metadata.entry_protocol"),
            "openai.responses"
        );
        assert_eq!(value("langfuse.trace.metadata.client_model"), "m");
    }

    /// The otelcol OTLP/HTTP receiver only accepts POSTs on /v1/traces; a
    /// bare-host endpoint must get the path appended, complete paths must be
    /// kept verbatim.
    #[test]
    fn normalize_endpoint_appends_traces_path() {
        assert_eq!(
            normalize_endpoint_path("http://otel-sink:4318"),
            "http://otel-sink:4318/v1/traces"
        );
        assert_eq!(
            normalize_endpoint_path("http://otel-sink:4318/"),
            "http://otel-sink:4318/v1/traces"
        );
        assert_eq!(
            normalize_endpoint_path("http://pk:sk@127.0.0.1:13000/api/public/otel/v1/traces"),
            "http://pk:sk@127.0.0.1:13000/api/public/otel/v1/traces"
        );
        assert_eq!(
            normalize_endpoint_path("http://host/api/public/otel"),
            "http://host/api/public/otel/v1/traces"
        );
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
