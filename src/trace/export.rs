//! OTLP/HTTP export of turn records (JSON encoding) to the configured
//! endpoint. Fail-open by design: bounded queue, drop on overflow or endpoint
//! failure, health counters observable — business traffic is never blocked.
//!
//! Shape since ATG #13: ONE batcher task owns the coalescing point (µs per
//! record) while the expensive half — serializing a ≥0.5 MB batch and POSTing
//! it (37.6 ms per batch of 32, measured) — runs in a bounded pool of flush
//! tasks on an export-owned multi-thread runtime. The pool is bounded by a
//! semaphore (`ATG_EXPORT_MAX_INFLIGHT`), so the loss point stays the
//! 1024-slot channel (`dropped`) and memory stays flat: the batcher parks on
//! a permit instead of spawning without bound.
// PANIC-AUDIT v0.3.13: audited file — serde_json writing into a locally-owned
// Vec (the ATG #13 writer path; failures are handled, never unwrapped),
// provably-bounded slices/arithmetic on locally-owned buffers (wire bodies
// capped by the capture layer), and a flush-task booking whose `Drop` runs
// during an unwind and therefore writes stderr best-effort (a panicking Drop
// would abort the process — the opposite of fail-open). The
// indexing/arithmetic lints are syntax-broad here; tracked in the
// PanicAudit issue.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use atg_model::TurnRecord;
use parking_lot::Mutex;
use serde::Serialize;
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

const QUEUE_CAPACITY: usize = 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Records per flush. Unchanged by ATG #13: the sink's cost is linear in the
/// batch's bytes, so a larger batch buys +4.8% and nothing else (#12 ruling 2).
const BATCH_MAX: usize = 32;
/// `ATG_EXPORT_MAX_INFLIGHT` (default 8): flush tasks in flight, i.e. the
/// memory bound (`MAX_INFLIGHT × batch payload` ≈ 126 MB at the measured
/// 15.7 MB/batch). This is a concurrency bound, NOT a queue-size knob: the
/// queue stays 1024 and remains the drop point.
const DEFAULT_MAX_INFLIGHT: usize = 8;
/// `ATG_EXPORT_WORKERS` (default 4): worker threads of the export-owned
/// runtime — the serialization parallelism (pingora's threads have no ambient
/// tokio runtime, so the export subsystem owns its own).
const DEFAULT_WORKERS: usize = 4;
const MAX_INFLIGHT_ENV: &str = "ATG_EXPORT_MAX_INFLIGHT";
const WORKERS_ENV: &str = "ATG_EXPORT_WORKERS";

use atg_model::{
    usage_details_json, ATTR_COMPLETION_START_TIME, ATTR_MODEL_NAME, ATTR_OBSERVATION_INPUT,
    ATTR_OBSERVATION_OUTPUT, ATTR_OBSERVATION_TYPE, ATTR_USAGE_DETAILS, ATTR_USER_ID,
    GENERATION_SPAN_NAME, LANGFUSE_TRACE_NAME, OBSERVATION_TYPE_GENERATION,
};

#[derive(Default)]
pub struct ExportHealth {
    pub exported: std::sync::atomic::AtomicU64,
    pub failed: std::sync::atomic::AtomicU64,
    pub dropped: std::sync::atomic::AtomicU64,
    /// Batch flushes aborted by a panic inside the export task
    /// (v0.3.8 seam: the task survives and keeps exporting).
    pub panicked: std::sync::atomic::AtomicU64,
    /// Batches inside the flush pool right now (accepted, not yet finished).
    /// A gauge, not a counter — it rises as the batcher dispatches and falls
    /// as flush tasks end, and its ceiling is the pool's permit count.
    pub inflight: std::sync::atomic::AtomicU64,
}

pub struct Exporter {
    tx: Option<mpsc::Sender<Arc<TurnRecord>>>,
    pub health: Arc<ExportHealth>,
    /// Effective pool width as configured at startup; 0 when export is
    /// disabled (same convention as `queue_capacity`). Self-attestation for
    /// load windows: a throughput number is only attributable to a knob
    /// setting if the instance reports the setting it actually ran with.
    max_inflight: usize,
    /// Effective export-runtime worker threads (0 when export is disabled).
    workers: usize,
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
        let max_inflight = env_positive_usize(MAX_INFLIGHT_ENV, DEFAULT_MAX_INFLIGHT);
        let workers = env_positive_usize(WORKERS_ENV, DEFAULT_WORKERS);
        let Some(endpoint) = endpoint.filter(|s| !s.trim().is_empty()) else {
            return Self {
                tx: None,
                health,
                max_inflight: 0,
                workers: 0,
            };
        };
        let (endpoint, auth_header) = split_basic_auth(&endpoint);
        let (tx, rx) = mpsc::channel::<Arc<TurnRecord>>(QUEUE_CAPACITY);
        let health2 = health.clone();
        // The gateway proxy runs on pingora's threads (no ambient tokio
        // runtime), so the exporter owns a dedicated runtime — multi-threaded
        // since ATG #13: the batcher needs its own task while flush tasks
        // serialize batches in parallel.
        std::thread::spawn(move || {
            // PANIC-AUDIT v0.3.8: exporter thread startup — a runtime
            // build failure is fatal by design and must abort the thread
            // (class ii, process startup path).
            #[allow(clippy::expect_used)]
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .expect("export runtime");
            rt.block_on(export_loop(
                endpoint,
                auth_header,
                rx,
                health2,
                max_inflight,
            ));
        });
        Self {
            tx: Some(tx),
            health,
            max_inflight,
            workers,
        }
    }

    /// Queue one record for export. Never blocks and never copies the record
    /// (the caller's `Arc` moves into the queue); drops (counted) when the
    /// queue is full or the export thread is gone.
    pub fn submit(&self, record: Arc<TurnRecord>) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(record) {
            Ok(()) => {}
            // Full: the 1024-slot queue is the backpressure point (unchanged).
            // Closed: the export thread is gone, so the record is as lost as a
            // dropped one — both count, as the pre-ATG#13 `Err(_)` arm did.
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.health.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Records currently buffered in the export queue (occupancy of the
    /// bounded channel; ATG issue #2 priority 4 — export backpressure was
    /// only visible as a drop counter). 0 when export is disabled.
    pub fn queue_depth(&self) -> usize {
        match &self.tx {
            Some(tx) => QUEUE_CAPACITY.saturating_sub(tx.capacity()),
            None => 0,
        }
    }

    /// Configured export queue capacity (0 when export is disabled).
    pub fn queue_capacity(&self) -> usize {
        match &self.tx {
            Some(_) => QUEUE_CAPACITY,
            None => 0,
        }
    }

    /// Effective flush-pool width (`ATG_EXPORT_MAX_INFLIGHT`); 0 = disabled.
    pub fn max_inflight(&self) -> usize {
        self.max_inflight
    }

    /// Effective export-runtime worker threads (`ATG_EXPORT_WORKERS`); 0 =
    /// disabled.
    pub fn workers(&self) -> usize {
        self.workers
    }
}

/// One export knob value: `(effective, fell_back)`. Mirrors
/// `parse_worker_threads`: a positive integer is taken as-is, anything else
/// falls back to the default. Zero must fall back — a 0-permit semaphore or a
/// 0-worker runtime would stall the export subsystem forever, which is the one
/// failure mode fail-open must not have.
fn parse_positive_usize(raw: &str, default: usize) -> (usize, bool) {
    match raw.trim().parse::<usize>() {
        Ok(n) if n > 0 => (n, false),
        _ => (default, true),
    }
}

/// Read one export knob from the environment; a fallback is reported once at
/// startup, because a silently ignored knob is how a measurement window ends
/// up running a setting nobody chose (v0.3.12 `ATG_WORKER_THREADS` precedent).
fn env_positive_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Err(_) => default,
        Ok(raw) => {
            let (value, fell_back) = parse_positive_usize(&raw, default);
            if fell_back {
                eprintln!("ATG: {name}={raw:?} is not a positive integer — using {default}");
            }
            value
        }
    }
}

/// One export destination: the URL (userinfo already split out into the
/// Authorization header). Shared by every flush task, so spawning a batch
/// copies no strings.
struct FlushTarget {
    endpoint: String,
    auth_header: Option<String>,
}

async fn export_loop(
    endpoint: String,
    auth_header: Option<String>,
    mut rx: mpsc::Receiver<Arc<TurnRecord>>,
    health: Arc<ExportHealth>,
    max_inflight: usize,
) {
    let client = reqwest_client();
    let target = Arc::new(FlushTarget {
        endpoint,
        auth_header,
    });
    // Memory bound and backpressure in one primitive: while every permit is
    // held the batcher parks on `acquire`, stops draining `rx`, and the
    // 1024-slot channel fills — the same loss point as before, but reached
    // only at the pool's real capacity instead of at a single flusher's.
    let permits = Arc::new(Semaphore::new(max_inflight));
    let mut pool: JoinSet<()> = JoinSet::new();
    let mut buf: Vec<Arc<TurnRecord>> = Vec::with_capacity(BATCH_MAX);
    let mut last_flush = Instant::now();
    loop {
        match tokio::time::timeout(BATCH_INTERVAL, rx.recv()).await {
            Ok(Some(rec)) => buf.push(rec),
            Ok(None) => break, // channel closed
            Err(_) => {}       // tick: flush if anything buffered
        }
        if buf.is_empty() || last_flush.elapsed() < BATCH_INTERVAL && buf.len() < BATCH_MAX {
            continue;
        }
        let batch = std::mem::replace(&mut buf, Vec::with_capacity(BATCH_MAX));
        last_flush = Instant::now();
        dispatch_batch(&permits, &mut pool, &client, &target, batch, &health).await;
        // Reap finished tasks: a JoinSet holds every completed task's output
        // until it is joined, so an unreaped pool would grow without bound.
        while let Some(joined) = pool.try_join_next() {
            observe_join(joined);
        }
    }
    // Shutdown (ATG #13 S4): the channel is closed and drained. Ship the tail
    // batch through the same bounded path, then wait for every in-flight batch
    // — the single-flusher code flushed the tail and abandoned whatever was
    // still in flight.
    if !buf.is_empty() {
        dispatch_batch(&permits, &mut pool, &client, &target, buf, &health).await;
    }
    while let Some(joined) = pool.join_next().await {
        observe_join(joined);
    }
}

/// Hand one batch to the flush pool.
///
/// Parks on a pool permit first: that await is the backpressure (producers
/// keep filling the bounded channel meanwhile, and `submit()` counts what
/// does not fit).
async fn dispatch_batch(
    permits: &Arc<Semaphore>,
    pool: &mut JoinSet<()>,
    client: &reqwest::Client,
    target: &Arc<FlushTarget>,
    batch: Vec<Arc<TurnRecord>>,
    health: &Arc<ExportHealth>,
) {
    let permit = match Arc::clone(permits).acquire_owned().await {
        Ok(permit) => permit,
        // Unreachable: this loop owns the semaphore and never closes it. If it
        // ever happened the batch would be booked as failed rather than
        // dropped uncounted (fail-open still counts).
        Err(_closed) => {
            health
                .failed
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            return;
        }
    };
    let records = batch.len();
    health.inflight.fetch_add(1, Ordering::Relaxed);
    let client = client.clone();
    let target = Arc::clone(target);
    let health = health.clone();
    pool.spawn(async move {
        // Held for the whole flush and released on the normal path and during
        // an unwind alike (a dropped permit frees the slot), so a panicking
        // batch can never wedge the pool.
        let _permit = permit;
        let mut booking = FlushBooking::enter(&health, records);
        flush_batch(&client, &target, &batch, &health).await;
        // Reached the end: `flush_batch` booked the batch itself (exported on
        // 2xx, failed otherwise). Disarm; the drop releases the gauge.
        booking.completed = true;
    });
}

/// One flush task's accounting bracket.
///
/// The v0.3.8 invariant — a panic in one flush must not stop the export
/// subsystem — is structural now (each batch is an independent `JoinSet`
/// task and the batcher keeps dispatching), but booking a panicked batch
/// (`failed += records`, `panicked += 1`) needs the batch length, which lives
/// only inside the task. `Drop` runs during the unwind, so the booking is
/// written there; the normal path disarms it.
struct FlushBooking {
    health: Arc<ExportHealth>,
    records: usize,
    completed: bool,
}

impl FlushBooking {
    fn enter(health: &Arc<ExportHealth>, records: usize) -> Self {
        Self {
            health: Arc::clone(health),
            records,
            completed: false,
        }
    }
}

impl Drop for FlushBooking {
    fn drop(&mut self) {
        self.health.inflight.fetch_sub(1, Ordering::Relaxed);
        if self.completed {
            return;
        }
        self.health
            .failed
            .fetch_add(self.records as u64, Ordering::Relaxed);
        self.health.panicked.fetch_add(1, Ordering::Relaxed);
        // Best-effort write: this Drop can run while unwinding, and a failed
        // write inside a panicking Drop aborts the process — the outage
        // fail-open exists to prevent. (`eprintln!` panics on a write error.)
        let _ = writeln!(
            std::io::stderr(),
            "OTLP export pool: flush panicked — batch dropped ({} records), export continues",
            self.records
        );
    }
}

/// Report a finished flush task. The batch of a panicking task is booked by
/// its own `FlushBooking` during the unwind (that is where the batch length
/// is known), so this seam only keeps the pool alive and observable.
fn observe_join(joined: Result<(), tokio::task::JoinError>) {
    match joined {
        Ok(()) => {}
        // Booked by the task's `FlushBooking` while unwinding; nothing left to
        // account for here.
        Err(err) if err.is_panic() => {}
        Err(err) => {
            eprintln!("OTLP export pool: flush task ended without completing: {err}");
        }
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
    target: &FlushTarget,
    batch: &[Arc<TurnRecord>],
    health: &ExportHealth,
) {
    let payload = build_otlp_body(batch);
    let mut req = client
        .post(normalize_endpoint_path(&target.endpoint))
        .header("content-type", "application/json")
        .header(
            atg_model::INGESTION_VERSION_HEADER,
            atg_model::INGESTION_VERSION,
        );
    if let Some(auth) = &target.auth_header {
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
                        display_endpoint(&target.endpoint)
                    );
                }
                Err(e) => {
                    eprintln!(
                        "OTLP export failed: endpoint={} error={e}",
                        display_endpoint(&target.endpoint)
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

/// One OTLP attribute (KeyValue). Field order is part of the byte contract:
/// the pinned implementation built `serde_json::Value` objects, whose maps are
/// BTreeMaps (serde_json without `preserve_order`), so every object came out
/// with sorted keys — "key" before "value" either way.
#[derive(Serialize)]
struct Attr<'a> {
    key: Cow<'a, str>,
    value: AttrValue<'a>,
}

/// The two attribute value shapes this exporter writes.
#[derive(Serialize)]
enum AttrValue<'a> {
    #[serde(rename = "stringValue")]
    Str(Cow<'a, str>),
    #[serde(rename = "arrayValue")]
    Array(ArrayValue<'a>),
}

#[derive(Serialize)]
struct ArrayValue<'a> {
    values: Vec<AttrValue<'a>>,
}

/// Attribute with a literal key: values that are plain field slices borrow,
/// computed ones (formatted tag, ISO-8601 stamp, nested JSON) are owned
/// through `Cow`.
fn attr<'a, K, V>(key: K, value: V) -> Attr<'a>
where
    K: Into<Cow<'a, str>>,
    V: Into<Cow<'a, str>>,
{
    Attr {
        key: key.into(),
        value: AttrValue::Str(value.into()),
    }
}

/// OTLP string-array attribute (Langfuse tags carry array semantics; see
/// modeltrace `otlpStringSlice` / `attribute.StringSlice`).
fn attr_array<'a, K>(key: K, values: Vec<AttrValue<'a>>) -> Attr<'a>
where
    K: Into<Cow<'a, str>>,
{
    Attr {
        key: key.into(),
        value: AttrValue::Array(ArrayValue { values }),
    }
}

/// One turn as an OTLP span. Field order = the pinned byte order (sorted, for
/// the same BTreeMap reason as `Attr`).
#[derive(Serialize)]
struct SpanJson<'a> {
    attributes: Vec<Attr<'a>>,
    #[serde(rename = "endTimeUnixNano")]
    end_time_unix_nano: String,
    kind: u8,
    name: &'static str,
    #[serde(rename = "spanId")]
    span_id: String,
    #[serde(rename = "startTimeUnixNano")]
    start_time_unix_nano: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<StatusJson<'a>>,
    #[serde(rename = "traceId")]
    trace_id: String,
}

/// AMB-7: native OTLP span status. Langfuse derives the observation's `level`
/// from `span.status.code` and its statusMessage from `span.status.message`.
#[derive(Serialize)]
struct StatusJson<'a> {
    code: u8,
    message: Cow<'a, str>,
}

#[derive(Serialize)]
struct ScopeJson {
    name: &'static str,
}

#[derive(Serialize)]
struct ScopeSpansJson<'a> {
    scope: ScopeJson,
    spans: Vec<SpanJson<'a>>,
}

#[derive(Serialize)]
struct ResourceJson {
    attributes: [Attr<'static>; 1],
}

#[derive(Serialize)]
struct ResourceSpansJson<'a> {
    resource: ResourceJson,
    #[serde(rename = "scopeSpans")]
    scope_spans: [ScopeSpansJson<'a>; 1],
}

#[derive(Serialize)]
struct OtlpJson<'a> {
    #[serde(rename = "resourceSpans")]
    resource_spans: [ResourceSpansJson<'a>; 1],
}

/// Minimal OTLP/HTTP JSON: one resourceSpans with a scopeSpans holding one
/// span per turn (session -> turn organization is expressed through the
/// session.id attribute; consumers group by it).
///
/// ATG #13 S1: written straight into the output buffer. The pinned version
/// built ~1200 `serde_json::Value` nodes per batch (32 turns × ~40
/// attributes), serialized them, and dropped the tree. Byte-for-byte
/// identical to it — `writer_matches_pinned_value_tree_byte_for_byte` is the
/// contract that keeps it that way.
fn build_otlp_body(batch: &[Arc<TurnRecord>]) -> Vec<u8> {
    let spans: Vec<SpanJson<'_>> = batch.iter().map(|r| span_json(r)).collect();
    let doc = OtlpJson {
        resource_spans: [ResourceSpansJson {
            resource: ResourceJson {
                attributes: [attr("service.name", "agent-trace-gateway")],
            },
            scope_spans: [ScopeSpansJson {
                scope: ScopeJson {
                    name: "agent-trace-gateway",
                },
                spans,
            }],
        }],
    };
    let mut out = Vec::with_capacity(body_size_hint(batch));
    // Infallible for these types (no custom Serialize, no non-string map keys,
    // no floats), but a failure must degrade rather than panic: an empty body
    // is a failed batch (counted by flush_batch), which is the fail-open
    // contract.
    if serde_json::to_writer(&mut out, &doc).is_err() {
        return Vec::new();
    }
    out
}

/// Capacity hint for the payload buffer. The body is dominated by the captured
/// content: `user_input`/`final_output` ride two attributes each (legacy key +
/// official observation key), the raw wire bodies once. Escaping has no fixed
/// ratio, so this is a hint, not a bound.
fn body_size_hint(batch: &[Arc<TurnRecord>]) -> usize {
    batch.iter().fold(0usize, |acc, r| {
        // Saturating arithmetic: this file carries a file-level arithmetic
        // allow for audited legacy code, and new code must not lean on it.
        let content = r
            .raw_request
            .len()
            .saturating_add(r.raw_response.len())
            .saturating_add(r.user_input.len().saturating_mul(2))
            .saturating_add(r.final_output.len().saturating_mul(2));
        acc.saturating_add(content).saturating_add(1024)
    })
}

/// One turn → one generation span. Attribute order is the pinned byte
/// contract; attribute *presence* rules (empty ⇒ omitted) are what the shape
/// tests pin.
fn span_json(r: &TurnRecord) -> SpanJson<'_> {
    // Explicit session only: modeltrace never writes session attributes when
    // no id exists (middleware.go session == ""), so empty-session turns must
    // not invent one on either line. Trace-level attributes (session/tags/
    // name) are copied onto every span in the trace (spec section 3) and are
    // appended AFTER the fixed block, in the pinned push order.
    let mut trace_extra: Vec<Attr<'_>> = Vec::with_capacity(12);
    // F2 harness wire (trace-level): tag harness:<name> +
    // langfuse.trace.metadata.{harness,…}.
    if !r.harness.is_empty() {
        trace_extra.push(attr("langfuse.trace.metadata.harness", r.harness.as_str()));
    }
    // Two-tier: the session-carrying dialect, independent of the
    // identity (omp borrows the claude-code dialect).
    if !r.dialect.is_empty() {
        trace_extra.push(attr("langfuse.trace.metadata.dialect", r.dialect.as_str()));
    }
    // Attribution-evidence audit: the UA the gateway actually saw.
    if !r.client_ua.is_empty() {
        trace_extra.push(attr(
            "langfuse.trace.metadata.client_ua",
            r.client_ua.as_str(),
        ));
    }
    // Client cancelled mid-turn (three-way error taxonomy): the turn records
    // normally with partial content; the marker is reconciliation material
    // (upstream may have drained/billed).
    if r.cancelled {
        trace_extra.push(attr("langfuse.trace.metadata.cancelled", "true"));
    }
    // Salted API-credential fingerprint: key-reuse correlation without the key
    // (16-hex salted sha256, plaintext never leaves the gateway).
    if !r.api_key_fp.is_empty() {
        trace_extra.push(attr(
            "langfuse.trace.metadata.client_key_fp",
            r.api_key_fp.as_str(),
        ));
    }
    if r.harness_anomaly {
        trace_extra.push(attr(
            "langfuse.trace.metadata.harness_protocol_anomaly",
            "true",
        ));
    }
    // §E ruling: synthetic (stitcher-minted) sessions are tagged so
    // Langfuse-side queries can exclude them from hit-rate numerators.
    if r.session_synthetic {
        trace_extra.push(attr("langfuse.trace.metadata.session_synthetic", "true"));
    }
    if r.harness_candidates.len() > 1 {
        trace_extra.push(attr(
            "langfuse.trace.metadata.harness_candidates",
            r.harness_candidates.join(","),
        ));
    }
    for (k, v) in &r.harness_enrich {
        trace_extra.push(attr(format!("langfuse.trace.metadata.{k}"), v.as_str()));
    }
    // P1-10: modeltrace-aligned trace metadata — the entry protocol and the
    // client-declared model (queryable cross-line).
    trace_extra.push(attr(
        "langfuse.trace.metadata.entry_protocol",
        r.protocol.as_str(),
    ));
    if !r.model_name.is_empty() {
        trace_extra.push(attr(
            "langfuse.trace.metadata.client_model",
            r.model_name.as_str(),
        ));
    }

    let mut attributes: Vec<Attr<'_>> = Vec::with_capacity(trace_extra.len().saturating_add(12));
    if !r.session_id.is_empty() {
        // P2-14: single official key — dual spelling converged.
        attributes.push(attr("langfuse.session.id", r.session_id.as_str()));
    }
    let tags: Vec<AttrValue<'_>> = if r.harness.is_empty() {
        vec![AttrValue::Str(Cow::Borrowed(
            atg_model::langfuse_trace_tag(),
        ))]
    } else {
        // line:<source> rides the resolved ATG_TRACE_TAG; the harness tag is
        // the orthogonal agent-attribution dimension.
        vec![
            AttrValue::Str(Cow::Borrowed(atg_model::langfuse_trace_tag())),
            AttrValue::Str(Cow::Owned(format!("harness:{}", r.harness))),
        ]
    };
    attributes.extend([
        attr("protocol", r.protocol.as_str()),
        attr("langfuse.trace.name", LANGFUSE_TRACE_NAME),
        attr_array("langfuse.trace.tags", tags),
        attr(ATTR_OBSERVATION_TYPE, OBSERVATION_TYPE_GENERATION),
        attr("user_input", r.user_input.as_str()),
        attr("final_output", r.final_output.as_str()),
        attr("raw_request", r.raw_request.as_str()),
        attr("raw_response", r.raw_response.as_str()),
        attr("breakpoint", if r.breakpoint { "true" } else { "false" }),
    ]);
    // P0-3: official observation content keys (UI panel reads these);
    // empty strings are omitted. LEGAL on generations (the mapping
    // table allows input/output on any observation type).
    if !r.user_input.is_empty() {
        attributes.push(attr(ATTR_OBSERVATION_INPUT, r.user_input.as_str()));
    }
    if !r.user_id.is_empty() {
        attributes.push(attr(ATTR_USER_ID, r.user_id.as_str()));
    }
    if !r.final_output.is_empty() {
        attributes.push(attr(ATTR_OBSERVATION_OUTPUT, r.final_output.as_str()));
    }
    if !r.model_name.is_empty() {
        attributes.push(attr(ATTR_MODEL_NAME, r.model_name.as_str()));
    }
    // P1-8: generation-exclusive completion start (ISO 8601 Z,
    // nanosecond precision) — first output byte on the wire
    // (streaming) or the request start (non-streaming).
    if let Some(ns) = r.completion_start_ns {
        attributes.push(attr(ATTR_COMPLETION_START_TIME, iso8601_z(ns)));
    }
    // Usage (exclusive buckets) — generation-only field, now on the
    // root generation itself.
    if let Some(u) = &r.usage {
        // G1: an all-zero usage passes the is_empty() gate but
        // serializes to "{}" — omit the attribute entirely
        // (zero ≙ unreported, same rule as the entry level).
        let details = usage_details_json(u);
        if details != "{}" {
            attributes.push(attr(ATTR_USAGE_DETAILS, details));
        }
    }
    if !r.tool_calls.is_empty() {
        let tool_calls_json = serde_json::to_string(&r.tool_calls).unwrap_or_default();
        attributes.push(attr("tool_calls", tool_calls_json));
    }
    attributes.extend(trace_extra);
    // P0-N1 (carried over): one random traceId + spanId per TURN.
    let trace_id = random_trace_id();
    let span_id = random_span_id();
    SpanJson {
        attributes,
        end_time_unix_nano: r.end_ns.to_string(),
        kind: 3,
        name: GENERATION_SPAN_NAME,
        span_id,
        start_time_unix_nano: r.start_ns.to_string(),
        status: r.error.as_ref().map(|err| StatusJson {
            // OTLP StatusCode::Error — Langfuse maps to level=ERROR.
            code: 2,
            message: Cow::Borrowed(err.as_str()),
        }),
        trace_id,
    }
}

/// Test shim: the production writer path over plain records, so the shape
/// tests below keep asserting on a `String` payload.
#[cfg(test)]
fn production_json(records: &[TurnRecord]) -> String {
    let batch: Vec<Arc<TurnRecord>> = records.iter().cloned().map(Arc::new).collect();
    String::from_utf8(build_otlp_body(&batch)).unwrap_or_default()
}

/// Pinned reference implementation (v0.3.12, text sha256 `c3edf8d3`): the same
/// payload built as a `serde_json::Value` tree. Test-only — the ATG #13
/// byte-equivalence test is the only consumer, and production sends what
/// `build_otlp_body` writes.
#[cfg(test)]
fn build_otlp_json_value_tree(batch: &[TurnRecord]) -> String {
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
                // line:<source> rides the resolved ATG_TRACE_TAG; the
                // harness tag is the orthogonal agent-attribution dimension.
                Some(t) => vec![atg_model::langfuse_trace_tag(), t.as_str()],
                None => vec![atg_model::langfuse_trace_tag()],
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

#[cfg(test)]
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

#[cfg(test)]
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
    #[cfg(test)]
    if let Some(bytes) = test_id_bytes(n) {
        return bytes;
    }
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

// Test-only deterministic id stream (ATG #13 A2): the byte-equivalence test
// serializes the same batch twice — once per implementation — and the
// trace/span ids are the only non-deterministic input. The stream is
// thread-local and installed only by that test, so no other test (and no
// production path) ever sees it.
#[cfg(test)]
thread_local! {
    static TEST_ID_STREAM: std::cell::RefCell<Option<u64>> = const { std::cell::RefCell::new(None) };
}

/// Test-only: draw the next id from the installed stream, or `None` when the
/// stream is not installed (the production path).
#[cfg(test)]
fn test_id_bytes(n: usize) -> Option<Vec<u8>> {
    use sha2::{Digest, Sha256};
    TEST_ID_STREAM.with(|cell| {
        let mut slot = cell.borrow_mut();
        let counter = slot.as_mut()?;
        let bytes = Sha256::digest(counter.to_be_bytes());
        *counter = counter.wrapping_add(1);
        Some(bytes.iter().copied().take(n).collect())
    })
}

/// Test-only: reseed the deterministic id stream (reproducible per run).
#[cfg(test)]
fn seed_test_ids() {
    TEST_ID_STREAM.with(|cell| *cell.borrow_mut() = Some(0));
}

/// Test-only: uninstall the deterministic stream.
#[cfg(test)]
fn clear_test_ids() {
    TEST_ID_STREAM.with(|cell| *cell.borrow_mut() = None);
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

    /// Batches in flight in the flush pool (ATG #13 A4 gauge). Read by the
    /// health and metrics endpoints; monotone per batch, not per record.
    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Relaxed)
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
    use atg_model::{ToolCall, TurnUsage};

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

    // ── ATG #13 A2: the writer path must not move a single byte ─────────────

    /// Knob nails (ATG #13 S3): the defaults are the ones the ticket
    /// specifies, a valid value is taken verbatim, and a bad value falls back
    /// (reported) instead of silently becoming something else.
    #[test]
    fn export_knob_parsing_nails() {
        assert_eq!(DEFAULT_MAX_INFLIGHT, 8, "ATG#13 S3 default");
        assert_eq!(DEFAULT_WORKERS, 4, "ATG#13 S3 default");
        assert_eq!(parse_positive_usize("8", DEFAULT_MAX_INFLIGHT), (8, false));
        assert_eq!(
            parse_positive_usize(" 12 ", DEFAULT_MAX_INFLIGHT),
            (12, false),
            "trimmed"
        );
        assert_eq!(parse_positive_usize("2", DEFAULT_WORKERS), (2, false));
        for bad in ["", "0", "garbage", "-1", "1.5", "99999999999999999999"] {
            assert_eq!(
                parse_positive_usize(bad, DEFAULT_MAX_INFLIGHT),
                (DEFAULT_MAX_INFLIGHT, true),
                "{bad:?} must fall back and say so"
            );
        }
    }

    /// Sample batches for the byte-equivalence test: one batch per boundary
    /// shape, so a failure names the shape that broke.
    ///
    /// Covered shapes: shell span (all four content fields empty — the
    /// capture-off shape, 374 of 674 spans in the measured window),
    /// session+content, escaping (`"`, `\`, LF, CR, tab, C0 control, DEL,
    /// solidus), multibyte UTF-8 (CJK, astral emoji, combining mark), usage
    /// absent / all-zero / partial / with total, tool_calls empty and
    /// non-empty, error status present/absent, breakpoint true/false, harness
    /// metadata (harness, dialect, client_ua, api_key_fp, cancelled, anomaly,
    /// synthetic, candidates, enrich incl. an exotic enrich key),
    /// completion-start present/absent, user_id and model_name absent/present,
    /// a full `BATCH_MAX` batch, and a ~200 KB payload.
    fn sample_batches() -> Vec<(&'static str, Vec<TurnRecord>)> {
        let mut batches: Vec<(&'static str, Vec<TurnRecord>)> = Vec::new();

        // 1. Shell span: every content field empty.
        batches.push(("shell-span", vec![record("")]));

        // 2. Session + content, usage absent, no error, breakpoint false.
        let mut full = record("sess-full");
        full.user_input = "hello world".to_string();
        full.final_output = "done".to_string();
        full.raw_request =
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_string();
        full.raw_response = r#"{"id":"x","choices":[]}"#.to_string();
        full.user_id = "user-1".to_string();
        full.model_name = "claude-sonnet-4".to_string();
        batches.push(("session-content", vec![full]));

        // 3. Escaping: quote, backslash, LF, CR, tab, C0 control, DEL, solidus
        //    (the last must stay unescaped) — in content, raw wire text and the
        //    error message.
        let mut esc = record("sess-esc");
        esc.user_input = "quote:\" back\\slash\nline\r\n tab\there \u{1}\u{7f} a/b".to_string();
        esc.final_output = "\"quoted\"".to_string();
        esc.raw_request = "{\"k\":\"\\n\"}".to_string();
        esc.error = Some("upstream said \"no\"\n\tpath\\dir".to_string());
        esc.breakpoint = true;
        batches.push(("escaping", vec![esc]));

        // 4. Multibyte UTF-8: CJK, astral emoji, ZWJ sequence, combining mark.
        let mut mb = record("sess-\u{4e2d}\u{6587}");
        mb.user_input = "\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c} \u{1f680} e\u{301}".to_string();
        mb.final_output = "\u{1f469}\u{200d}\u{1f4bb} \u{2705}".to_string();
        mb.tool_calls.push(ToolCall {
            name: "\u{5de5}\u{5177}".to_string(),
            arguments: "{\"\u{952e}\":\"\u{503c}\"}".to_string(),
        });
        batches.push(("multibyte", vec![mb]));

        // 5. Usage shapes: absent, all-zero (omitted), partial, with total.
        let mut zero = record("sess-u0");
        zero.usage = Some(TurnUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            total_tokens: Some(0),
            ..Default::default()
        });
        let mut partial = record("sess-u1");
        partial.usage = Some(TurnUsage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            cache_read_tokens: Some(3),
            ..Default::default()
        });
        let mut with_total = record("sess-u2");
        with_total.usage = Some(TurnUsage {
            input_tokens: Some(5),
            total_tokens: Some(5),
            ..Default::default()
        });
        batches.push((
            "usage-shapes",
            vec![record("sess-none"), zero, partial, with_total],
        ));

        // 6. tool_calls empty (batch 1) vs non-empty (batch 2, nested JSON that
        //    itself carries an escape).
        let mut tools = record("sess-tools");
        tools.tool_calls = vec![
            ToolCall {
                name: "read".to_string(),
                arguments: "{\"path\":\"/tmp/a\\\"b\"}".to_string(),
            },
            ToolCall {
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            },
        ];
        batches.push(("tool-calls", vec![tools, record("sess-no-tools")]));

        // 7. Error status present (with escaping) next to an errorless span.
        let mut err = record("sess-err");
        err.error = Some("response.failed: upstream 500 \"boom\"\n".to_string());
        batches.push(("error-status", vec![err, record("sess-ok")]));

        // 8. Every conditional trace-metadata pair, including an enrich key
        //    that needs escaping.
        let mut h = record("sess-h");
        h.harness = "omp".to_string();
        h.dialect = "claude-code".to_string();
        h.client_ua = "omp/18.1.0".to_string();
        h.api_key_fp = "0123456789abcdef".to_string();
        h.cancelled = true;
        h.harness_anomaly = true;
        h.session_synthetic = true;
        h.harness_candidates = vec!["claude-code".to_string(), "codex".to_string()];
        h.harness_enrich
            .push(("cc_account".to_string(), "acc-1".to_string()));
        h.harness_enrich
            .push(("odd key\u{1}".to_string(), "v\"1".to_string()));
        batches.push(("harness-metadata", vec![h]));

        // 9. Harness tag alone; one candidate must be omitted (not joined).
        let mut h1 = record("sess-h1");
        h1.harness = "codex".to_string();
        h1.harness_candidates = vec!["codex".to_string()];
        batches.push(("harness-tag-only", vec![h1]));

        // 10. Completion stamp present (nanosecond precision) vs absent.
        let mut cs = record("sess-cs");
        cs.completion_start_ns = Some(1_788_912_000_000_000_042);
        batches.push(("completion-stamp", vec![cs, record("sess-no-cs")]));

        // 11. A full batch — the unit the flush pool actually serializes —
        //     mixing shells and content.
        let mixed: Vec<TurnRecord> = (0..BATCH_MAX)
            .map(|i| {
                let mut r = record(&format!("sess-{i}"));
                if i % 2 == 0 {
                    r.user_input = format!("turn {i} \u{1f600}");
                    r.raw_request = format!("{{\"i\":{i}}}");
                }
                r
            })
            .collect();
        batches.push(("full-batch-32", mixed));

        // 12. Large payload, the measured production shape (0.16 MB input →
        //     0.49 MB body): exercises the buffer-size hint and multi-KB
        //     attribute values.
        let mut big = record("sess-big");
        big.raw_request = "x".repeat(160 * 1024);
        big.raw_response = "\u{4e2d}".repeat(20 * 1024);
        big.user_input = "u".repeat(8 * 1024);
        big.final_output = "o".repeat(8 * 1024);
        batches.push(("large-payload", vec![big]));

        batches
    }

    fn batch_of(records: &[TurnRecord]) -> Vec<Arc<TurnRecord>> {
        records.iter().cloned().map(Arc::new).collect()
    }

    /// First-difference report — a 500 KB payload is unreadable as a plain
    /// `assert_eq!` diff.
    fn assert_same_bytes(label: &str, ours: &[u8], pinned: &[u8]) {
        if ours == pinned {
            return;
        }
        let at = ours
            .iter()
            .zip(pinned.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| ours.len().min(pinned.len()));
        let lo = at.saturating_sub(60);
        let hi = at + 60;
        let window =
            |b: &[u8]| String::from_utf8_lossy(&b[lo.min(b.len())..hi.min(b.len())]).into_owned();
        panic!(
            "{label}: writer path differs from the pinned value tree at byte {at} \
             (ours {} bytes, pinned {} bytes)\n  ours  : {:?}\n  pinned: {:?}",
            ours.len(),
            pinned.len(),
            window(ours),
            window(pinned)
        );
    }

    /// ATG #13 A2 (the ticket's correctness proof): for the same input batch,
    /// `build_otlp_body` and the pinned v0.3.12 value-tree implementation must
    /// emit identical bytes — key order, escaping, number rendering and
    /// omission rules included.
    ///
    /// The trace/span ids are the only random input; both runs draw them from
    /// the same seeded thread-local stream, so nothing is normalized away.
    #[test]
    fn writer_matches_pinned_value_tree_byte_for_byte() {
        for (label, records) in sample_batches() {
            let batch = batch_of(&records);
            let ours = {
                seed_test_ids();
                build_otlp_body(&batch)
            };
            let pinned = {
                seed_test_ids();
                build_otlp_json_value_tree(&records).into_bytes()
            };
            clear_test_ids();
            assert_same_bytes(label, &ours, &pinned);
        }
    }

    /// The samples must actually carry the shapes they are named for: if one
    /// silently stopped exercising its shape, byte-equivalence would still hold
    /// and the A2 test would prove nothing. Asserted on the production bytes.
    #[test]
    fn samples_exercise_the_named_shapes() {
        let body = |label: &str| -> String {
            let (_, records) = sample_batches()
                .into_iter()
                .find(|(l, _)| *l == label)
                .unwrap_or_else(|| panic!("no sample batch {label}"));
            String::from_utf8(build_otlp_body(&batch_of(&records))).unwrap_or_default()
        };
        let spans = |label: &str| -> Vec<serde_json::Value> {
            let payload: serde_json::Value = serde_json::from_str(&body(label)).unwrap();
            payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        };
        let value = |span: &serde_json::Value, key: &str| -> Option<String> {
            span_attr(span, key)
                .and_then(|v| v["stringValue"].as_str())
                .map(str::to_string)
        };

        // Shell span: the four content attributes are present and empty.
        let shell = spans("shell-span");
        for key in ["user_input", "final_output", "raw_request", "raw_response"] {
            assert_eq!(
                value(&shell[0], key).as_deref(),
                Some(""),
                "shell span must carry an empty {key}: {}",
                shell[0]
            );
        }

        // Escaping: the body has no raw LF (every LF is escaped) and the
        // escaped text round-trips losslessly through a JSON parse.
        let escaping = body("escaping");
        assert!(
            !escaping.contains('\n'),
            "LF must be escaped, never emitted raw"
        );
        assert!(
            escaping.contains("\\u0001"),
            "C0 controls must be \\u-escaped: {escaping}"
        );
        let esc_span = &spans("escaping")[0];
        assert_eq!(
            value(esc_span, "user_input").as_deref(),
            Some("quote:\" back\\slash\nline\r\n tab\there \u{1}\u{7f} a/b"),
            "escaped content must round-trip"
        );
        assert_eq!(
            esc_span["status"]["message"],
            "upstream said \"no\"\n\tpath\\dir"
        );
        assert_eq!(value(esc_span, "breakpoint").as_deref(), Some("true"));

        // Multibyte: non-ASCII rides the payload as raw UTF-8.
        let multibyte = body("multibyte");
        assert!(
            multibyte.contains("\u{4f60}\u{597d}\u{ff0c}"),
            "CJK rides the payload as raw UTF-8"
        );
        assert!(
            multibyte.contains("\u{1f680}"),
            "astral chars stay raw UTF-8"
        );
        assert!(multibyte.contains("e\u{301}"), "combining mark stays raw");

        // Usage omission rules.
        let usage = spans("usage-shapes");
        assert!(
            span_attr(&usage[0], ATTR_USAGE_DETAILS).is_none(),
            "absent usage: no attribute"
        );
        assert!(
            span_attr(&usage[1], ATTR_USAGE_DETAILS).is_none(),
            "all-zero usage: attribute omitted"
        );
        assert_eq!(
            value(&usage[2], ATTR_USAGE_DETAILS).as_deref(),
            Some(r#"{"cache_read_input_tokens":3,"input":12,"output":7}"#)
        );
        assert_eq!(
            value(&usage[3], ATTR_USAGE_DETAILS).as_deref(),
            Some(r#"{"input":5,"total":5}"#)
        );

        // tool_calls present only when non-empty, and lossless as nested JSON.
        let tools = spans("tool-calls");
        assert_eq!(
            value(&tools[0], "tool_calls").as_deref(),
            Some(
                r#"[{"name":"read","arguments":"{\"path\":\"/tmp/a\\\"b\"}"},{"name":"bash","arguments":"{}"}]"#
            ),
            "tool_calls ride as an escaped nested JSON string"
        );
        assert!(span_attr(&tools[1], "tool_calls").is_none());

        // Error status present/absent.
        let errs = spans("error-status");
        assert_eq!(errs[0]["status"]["code"], 2);
        assert!(
            errs[1]["status"].is_null(),
            "no error ⇒ no status: {}",
            errs[1]
        );

        // Harness metadata + the two-element tag list.
        let h = &spans("harness-metadata")[0];
        assert_eq!(
            value(h, "langfuse.trace.metadata.harness").as_deref(),
            Some("omp")
        );
        assert_eq!(
            value(h, "langfuse.trace.metadata.harness_candidates").as_deref(),
            Some("claude-code,codex")
        );
        assert_eq!(
            value(h, "langfuse.trace.metadata.odd key\u{1}").as_deref(),
            Some("v\"1")
        );
        let tags = span_attr(h, "langfuse.trace.tags").unwrap()["arrayValue"]["values"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(tags.len(), 2, "line + harness tags: {tags:?}");
        assert_eq!(tags[1]["stringValue"], "harness:omp");
        // One candidate ⇒ the joined attribute is omitted.
        let h1 = &spans("harness-tag-only")[0];
        assert!(span_attr(h1, "langfuse.trace.metadata.harness_candidates").is_none());

        // Completion stamp present/absent.
        let cs = spans("completion-stamp");
        assert_eq!(
            value(&cs[0], ATTR_COMPLETION_START_TIME).as_deref(),
            Some("2026-09-09T00:00:00.000000042Z")
        );
        assert!(span_attr(&cs[1], ATTR_COMPLETION_START_TIME).is_none());

        // The full batch is 32 spans; the large payload is a multi-hundred-KB
        // body that still parses.
        assert_eq!(spans("full-batch-32").len(), BATCH_MAX);
        let large = body("large-payload");
        assert!(
            large.len() > 200 * 1024,
            "large sample: {} bytes",
            large.len()
        );
        assert!(serde_json::from_str::<serde_json::Value>(&large).is_ok());
    }

    /// G1: langfuse.* vocabulary present with aligned values; tags is an
    /// OTLP string-array attribute.
    #[test]
    fn span_carries_langfuse_vocabulary() {
        let payload: serde_json::Value =
            serde_json::from_str(&production_json(&[record("sess-1")])).unwrap();
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
        // v0.3.10 combination pin: line:<source> (ATG_TRACE_TAG) and
        // harness:<name> are orthogonal dimensions — both ride the same
        // span when a harness is attributed.
        let mut r = record("sess-tags");
        r.harness = "omp".to_string();
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let tags = span_attr(&spans[0], "langfuse.trace.tags")
            .and_then(|v| v["arrayValue"]["values"].as_array())
            .unwrap_or_else(|| panic!("tags must be an OTLP array"));
        assert_eq!(tags.len(), 2, "line + harness both present: {tags:?}");
        assert!(
            tags.iter().any(|t| t["stringValue"] == "line:atg"),
            "line:<source> rides ATG_TRACE_TAG resolution (default here)"
        );
        assert!(
            tags.iter().any(|t| t["stringValue"] == "harness:omp"),
            "harness dimension unchanged by tag resolution"
        );
    }

    /// G1 (empty-session variant): session attributes are omitted entirely.
    #[test]
    fn empty_session_omits_session_attributes() {
        let payload: serde_json::Value =
            serde_json::from_str(&production_json(&[record("")])).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&records)).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&records)).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
            let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
        let payload: serde_json::Value = serde_json::from_str(&production_json(&[r])).unwrap();
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
