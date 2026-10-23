// agent-trace-gateway library: protocol interpretation + gateway app.
pub mod config;
pub mod config_store;
pub mod engine;
pub mod metrics;
pub mod trace;

pub mod gateway_app {
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use pingora::http::ResponseHeader;
    use pingora::prelude::*;
    use pingora::proxy::{http_proxy, FailToProxy, ProxyHttp, Session};
    use pingora::upstreams::peer::HttpPeer;
    use std::net::ToSocketAddrs;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::config::parse_upstream;
    use crate::trace::store::TraceStore;
    use crate::trace::unpack;

    /// Session-error classification — THREE outcomes by direction
    /// (user ruling, v0.3.5):
    ///
    /// 1. IdleNoise — teardown AFTER the response was fully delivered:
    ///    debug-grade noise, no marker at all (production evidence:
    ///    sub2api idle-timeout RST post-response, cause context
    ///    "during HTTP idle state"). Two discriminants:
    ///    (a) FAIL-SAFE BELT: the cause chain carries Pingora's
    ///    "during HTTP idle state" context — that window is
    ///    post-response by definition, so it cannot fire on a
    ///    truncated response; overlaps (b) on the production
    ///    signature and holds if the structural gate regresses.
    ///    (b) STRUCTURAL GATE: response ran to end_of_stream (headers
    ///    alone prove nothing) AND 2xx AND a downstream-sourced
    ///    error — downstream activity then can only be the idle
    ///    next-request probe.
    /// 2. ClientCancelled — a downstream-sourced session error that is
    ///    NOT idle teardown: the CLIENT hung up mid-turn (write side
    ///    failing to deliver, or read side closed before end_of_stream).
    ///    The turn records normally with its partial content plus a
    ///    cancelled=true metadata marker — no error marker, no ERROR
    ///    level (reconciliation material: the upstream may already have
    ///    drained/billed the request; production precedent).
    /// 3. ProxyError — everything else (upstream-sourced interruption,
    ///    half bodies, 5xx, dead connections): the existing failure
    ///    classification, unchanged.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum SessionErrorClass {
        IdleNoise,
        ClientCancelled,
        ProxyError,
    }

    fn classify_session_error(
        err_text: &str,
        downstream_sourced: bool,
        resp_status: u16,
        response_complete: bool,
    ) -> SessionErrorClass {
        if err_text.contains("during HTTP idle state")
            || (downstream_sourced && response_complete && (200..300).contains(&resp_status))
        {
            return SessionErrorClass::IdleNoise;
        }
        if downstream_sourced {
            return SessionErrorClass::ClientCancelled;
        }
        SessionErrorClass::ProxyError
    }

    /// Discriminator-level entry for tests (fabricating pingora Errors is
    /// impractical; the classifier's inputs are exactly these).
    #[doc(hidden)]
    pub fn __classify_session_error(
        err_text: &str,
        downstream_sourced: bool,
        resp_status: u16,
        response_complete: bool,
    ) -> SessionErrorClass {
        classify_session_error(err_text, downstream_sourced, resp_status, response_complete)
    }

    pub struct Gateway {
        /// The configuration store: every hot value is read from a
        /// per-request snapshot (one `snapshot()` per request, taken in
        /// `new_ctx` and carried in `Ctx`).
        pub config_store: Arc<crate::config_store::ConfigStore>,
        /// Upstream HTTP client for the owned relay (v0.3.6): pingora's
        /// response pump structurally aborts the upstream the moment the
        /// downstream dies, which makes a client-disconnect policy
        /// (drain switch) unenforceable inside the ProxyHttp hooks — so
        /// LLM API requests are relayed by the gateway itself instead.
        /// Plain client (no resolve pins); a pinned client for the current
        /// SNI entry is cached in `pinned_relay` and rebuilt when the
        /// config's upstream/SNI pair changes.
        pub http: reqwest::Client,
        /// Cached pinned relay client (BLOCK-A): (SNI name, pinned address)
        /// → client built with the matching resolve entry. Rebuilt when the
        /// config's upstream/sni change (hot keys); None while no pinning
        /// applies.
        pub pinned_relay:
            parking_lot::RwLock<Option<((String, std::net::SocketAddr), reqwest::Client)>>,
        /// Capture mode (structural, compile-time domain): trace.mode=off
        /// (bench builds only) skips capture and parsing — records degrade
        /// to timing/error/cancel shells, the body path is a near-pure
        /// forward (low-cost tap, v0.3.11). Pinned at startup; a reload
        /// reports a change as restart_required.
        pub trace_off: bool,
        /// The running listen address (structural, pinned at startup —
        /// the structural self-attestation for /__atg/config).
        pub listen: String,
        /// Hot knobs whose CURRENT values ride atomics re-seeded by the
        /// reload path (the per-request reads come from the ctx snapshot;
        /// these are the values the components apply without a snapshot
        /// round-trip).
        pub cap: Arc<crate::trace::capture::CaptureCap>,
        pub stitcher: Arc<crate::trace::prefix::PrefixStitcher>,
        pub store: TraceStore,
        pub exporter: crate::trace::export::Exporter,
        /// Frames that failed JSON parse during SSE unpack (observability;
        /// fail-open — never blocks).
        pub failed_frames: std::sync::atomic::AtomicU64,
        /// Landscape metrics (harness design §3): total turns, turns with a
        /// session id, turns with a harness attribution. Denominator
        /// filtering (session-semantics traffic only) is query-side.
        pub turns_total: std::sync::atomic::AtomicU64,
        /// Loose path matches (endpoint-variant detection) — counted per
        /// loose-hit request regardless of whether a record was produced
        /// (config debugging: base URLs without /v1).
        pub loose_path_matches: std::sync::atomic::AtomicU64,
        pub turns_with_session: std::sync::atomic::AtomicU64,
        pub turns_with_harness: std::sync::atomic::AtomicU64,
        /// Header-evidence entries dropped by the per-turn byte budget
        /// (`HDR_EVIDENCE_BUDGET`). The allowlist is closed, but values are
        /// client-controlled, so overflow must be countable — not silent.
        pub hdr_evidence_dropped: std::sync::atomic::AtomicU64,
        /// Effective pingora worker-thread count at startup (self-attestation
        /// for health/metrics; see run()).
        pub worker_threads: usize,
        /// Observability gauges (v0.3.12, ATG issue #2). `inflight` counts the
        /// requests inside the gateway — pingora's per-request `new_ctx` and
        /// `logging` hooks are the guaranteed brackets. `awaiting_upstream`
        /// counts the subset that has not yet seen upstream response headers:
        /// requests sitting in the gateway's queue/runtime AND waiting on the
        /// upstream's first byte ("where did it wait", the number the hop
        /// ladder cannot attribute). `inflight_high_water` is the monotonic
        /// max of `inflight`.
        pub inflight: std::sync::atomic::AtomicU64,
        pub inflight_high_water: std::sync::atomic::AtomicU64,
        pub awaiting_upstream: std::sync::atomic::AtomicU64,
        /// Per-stage latency histograms (v0.3.12): ctx start -> upstream
        /// response headers (`wait_upstream`), ctx start -> first downstream
        /// body byte (`time_to_first_byte`, the ATG-internal TTFB the matrix
        /// reads from records, aggregated), upstream headers -> body end
        /// (`delivery`), body end -> record finalized (`finalize`, the
        /// decode/capture/encode tail). Fixed-bucket relaxed atomics: no
        /// allocation, no locks, a handful of relaxed operations per request.
        pub stage_wait_upstream: crate::metrics::Histogram,
        pub stage_time_to_first_byte: crate::metrics::Histogram,
        pub stage_delivery: crate::metrics::Histogram,
        pub stage_finalize: crate::metrics::Histogram,
    }

    impl Gateway {
        fn push_record(&self, record: atg_model::TurnRecord) {
            self.store.push(record.clone());
            // ATG #13 S2: the export queue takes the record by `Arc` — the
            // exchange used to deep-copy the whole turn (raw_request /
            // raw_response can reach 839 KB / 333 KB) on the request path.
            self.exporter.submit(Arc::new(record));
        }

        /// Request entry (pingora `new_ctx`, exactly once per request):
        /// inflight/awaiting gauges plus the high-water mark.
        fn enter_request(&self) {
            use std::sync::atomic::Ordering::Relaxed;
            let in_flight = self.inflight.fetch_add(1, Relaxed).saturating_add(1);
            self.awaiting_upstream.fetch_add(1, Relaxed);
            self.inflight_high_water.fetch_max(in_flight, Relaxed);
        }

        /// Upstream response headers observed: the request leaves the
        /// "awaiting upstream first byte" set. Latched on `upstream_head_ns`
        /// so the decrement happens exactly once (the latch is the stage
        /// boundary too); the never-arrived arm lives in `exit_request`.
        fn mark_upstream_head(&self, ctx: &mut Ctx) {
            if ctx.upstream_head_ns.is_none() {
                ctx.upstream_head_ns = Some(now_ns());
                // `try_update` is the current spelling: CI tracks `rust:1`
                // (newer than the local devshell), where `fetch_update` is
                // deprecated and `-D warnings` makes it fatal.
                let _ = self.awaiting_upstream.try_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(1)),
                );
            }
        }

        /// Request end (pingora `logging`, called exactly once per request):
        /// both gauges release, and identified protocol paths record one
        /// observation per stage. Self-probes (/__atg/*) and unknown paths
        /// keep the gauges balanced but stay out of the stage histograms —
        /// they are not LLM turns and would skew the latency picture.
        fn exit_request(&self, ctx: &Ctx, identified: bool) {
            use std::sync::atomic::Ordering::Relaxed;
            let _ = self
                .inflight
                .try_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)));
            if ctx.upstream_head_ns.is_none() {
                // Upstream headers never arrived (connect failure, client
                // abort during upload, ...): release the awaiting slot here.
                let _ = self
                    .awaiting_upstream
                    .try_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(1)));
            }
            if !identified {
                return;
            }
            let end = now_ns();
            if let Some(head) = ctx.upstream_head_ns {
                self.stage_wait_upstream
                    .observe(head.saturating_sub(ctx.start_ns));
            }
            if let Some(first) = ctx.first_output_ns {
                self.stage_time_to_first_byte
                    .observe(first.saturating_sub(ctx.start_ns));
            }
            if let (Some(head), Some(body_end)) = (ctx.upstream_head_ns, ctx.body_end_ns) {
                self.stage_delivery.observe(body_end.saturating_sub(head));
            }
            if let Some(body_end) = ctx.body_end_ns {
                self.stage_finalize.observe(end.saturating_sub(body_end));
            }
        }

        /// Prometheus text exposition (v0.3.12, ATG issue #2). One block per
        /// family; instance attribution rides `atg_info`'s labels (several
        /// ATG instances run side by side — the scrape target names the
        /// instance, the labels attest what was scraped).
        fn render_metrics(&self) -> String {
            use std::sync::atomic::Ordering::Relaxed;
            let snap = self.config_store.snapshot();
            let (exported, failed, dropped, panicked) = self.exporter.health.snapshot();
            let (store_wait_ns, store_hold_ns) = self.store.lock_ns_totals();
            let mut out = String::with_capacity(4096);
            out.push_str("# HELP atg_info Build and runtime identity of this gateway instance.\n");
            out.push_str("# TYPE atg_info gauge\n");
            out.push_str("atg_info{version=\"");
            out.push_str(&crate::metrics::escape_label_value(env!(
                "CARGO_PKG_VERSION"
            )));
            out.push_str("\",variant=\"");
            out.push_str(variant_label());
            out.push_str("\",trace_mode=\"");
            out.push_str(trace_mode_label(self.trace_off));
            out.push_str("\",trace_tag=\"");
            out.push_str(&crate::metrics::escape_label_value(&snap.cfg.trace.tag));
            out.push_str("\"} 1\n");
            let gauges: [(&str, &str, u64); 11] = [
                (
                    "atg_requests_inflight",
                    "Requests currently inside the gateway (accepted, not yet logged).",
                    self.inflight.load(Relaxed),
                ),
                (
                    "atg_requests_awaiting_upstream",
                    "In-flight requests that have not yet seen upstream response headers (gateway queue plus upstream first-byte wait).",
                    self.awaiting_upstream.load(Relaxed),
                ),
                (
                    "atg_requests_inflight_high_water",
                    "Monotonic maximum of atg_requests_inflight since startup.",
                    self.inflight_high_water.load(Relaxed),
                ),
                (
                    "atg_worker_threads",
                    "Effective pingora worker-thread count at startup.",
                    self.worker_threads as u64,
                ),
                (
                    "atg_export_queue_depth",
                    "Turn records buffered in the OTLP export queue.",
                    self.exporter.queue_depth() as u64,
                ),
                (
                    "atg_export_queue_capacity",
                    "OTLP export queue capacity (0 = export disabled).",
                    self.exporter.queue_capacity() as u64,
                ),
                (
                    "atg_export_inflight",
                    "OTLP export batches currently in the flush pool (gauge; bounded by export.max_inflight).",
                    self.exporter.health.inflight(),
                ),
                (
                    "atg_export_max_inflight",
                    "Effective flush-pool width (export.max_inflight; 0 = export disabled).",
                    self.exporter.max_inflight() as u64,
                ),
                (
                    "atg_export_workers",
                    "Effective export-runtime worker threads (export.workers; structural, fixed at startup).",
                    self.exporter.workers() as u64,
                ),
                (
                    "atg_stitch_entries",
                    "Prefix-stitch chains currently held (drives the stitcher's O(table) sweep cost — ATG#5).",
                    self.stitcher.entries() as u64,
                ),
                (
                    "atg_stitch_capacity",
                    "Configured prefix-stitch LRU capacity.",
                    self.stitcher.capacity() as u64,
                ),
            ];
            for (name, help, value) in gauges {
                crate::metrics::render_gauge(&mut out, name, help, value);
            }
            let counters: [(&str, &str, u64); 17] = [
                (
                    "atg_turns_total",
                    "Requests that produced a traced turn record.",
                    self.turns_total.load(Relaxed),
                ),
                (
                    "atg_turns_with_session_total",
                    "Turns carrying an observed (non-synthetic) session id.",
                    self.turns_with_session.load(Relaxed),
                ),
                (
                    "atg_turns_with_harness_total",
                    "Turns attributed to a known harness identity.",
                    self.turns_with_harness.load(Relaxed),
                ),
                (
                    "atg_hdr_evidence_dropped_total",
                    "Header-evidence entries dropped by the per-turn byte budget.",
                    self.hdr_evidence_dropped.load(Relaxed),
                ),
                (
                    "atg_loose_path_matches_total",
                    "Requests matching a protocol path loosely (endpoint-variant detection).",
                    self.loose_path_matches.load(Relaxed),
                ),
                (
                    "atg_failed_frames_total",
                    "SSE frames that failed JSON parse during unpack (fail-open).",
                    self.failed_frames.load(Relaxed),
                ),
                (
                    "atg_store_dropped_total",
                    "Turn records evicted by the bounded in-process store.",
                    self.store.dropped(),
                ),
                (
                    "atg_exported_total",
                    "Turn records accepted by the OTLP endpoint.",
                    exported,
                ),
                (
                    "atg_export_failed_total",
                    "Turn records whose OTLP export attempt failed.",
                    failed,
                ),
                (
                    "atg_export_dropped_total",
                    "Turn records dropped because the export queue was full.",
                    dropped,
                ),
                (
                    "atg_export_panicked_batches_total",
                    "Export batches aborted by a panic inside the export task.",
                    panicked,
                ),
                (
                    "atg_stitch_expired_total",
                    "Prefix-stitch chains dropped by TTL expiry.",
                    self.stitcher.expired_total(),
                ),
                (
                    "atg_stitch_evicted_total",
                    "Prefix-stitch chains dropped by the capacity LRU.",
                    self.stitcher.evicted_total(),
                ),
                (
                    "atg_stitch_wait_ns_total",
                    "Cumulative nanoseconds blocked on the prefix-stitch lock (contention).",
                    self.stitcher.lock_wait_ns_total(),
                ),
                (
                    "atg_stitch_hold_ns_total",
                    "Cumulative nanoseconds held inside the prefix-stitch lock (critical section).",
                    self.stitcher.lock_hold_ns_total(),
                ),
                (
                    "atg_store_wait_ns_total",
                    "Cumulative nanoseconds blocked on the trace-store lock (contention).",
                    store_wait_ns,
                ),
                (
                    "atg_store_hold_ns_total",
                    "Cumulative nanoseconds held inside the trace-store lock (per-request push).",
                    store_hold_ns,
                ),
            ];
            for (name, help, value) in counters {
                crate::metrics::render_counter(&mut out, name, help, value);
            }
            self.stage_wait_upstream.render(
                &mut out,
                "atg_stage_wait_upstream_seconds",
                "Request start to upstream response headers (gateway queueing plus upstream first-byte latency).",
            );
            self.stage_time_to_first_byte.render(
                &mut out,
                "atg_stage_time_to_first_byte_seconds",
                "Request start to the first response body byte relayed downstream (ATG-internal TTFB).",
            );
            self.stage_delivery.render(
                &mut out,
                "atg_stage_delivery_seconds",
                "Upstream response headers to response body end (stream duration as seen by ATG).",
            );
            self.stage_finalize.render(
                &mut out,
                "atg_stage_finalize_seconds",
                "Response body end to record finalized (decode/capture/encode tail).",
            );
            out
        }

        /// Enter the drain window (v0.3.6): seed the incremental SSE
        /// accumulator with everything captured while the client was alive
        /// — drain final_output/usage must include the delivered prefix,
        /// not just post-death bytes — and return the drain deadline.
        /// Idempotent: the accumulator is seeded once.
        #[allow(clippy::arithmetic_side_effects)] // Instant + Duration: overflow is +584 years
        fn begin_drain(
            &self,
            ctx: &mut Ctx,
            d: &'static atg_protocol::ProtocolDescriptor,
        ) -> tokio::time::Instant {
            if ctx.drain_acc.is_none() && unpack::looks_like_sse(&ctx.resp_content_type) {
                let mut acc = crate::engine::SseAccum::default();
                crate::engine::drain_feed(&mut acc, d, &mut ctx.drain_tail, &ctx.resp_buf);
                ctx.drain_acc = Some(acc);
            }
            tokio::time::Instant::now() + Duration::from_secs(ctx.snap.cfg.drain.timeout_secs)
        }

        /// Bounded raw capture during drain (v0.3.8 helper): bytes stop at
        /// the capture cap; overflow is counted. Bounds are provable —
        /// take = min(cap − buf.len(), chunk.len()) — the allow covers the
        /// lint's syntax-broad view of the provable slice/subtraction.
        #[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
        fn drain_capture(&self, ctx: &mut Ctx, bytes: &[u8]) {
            let cap = self.cap.max_bytes();
            let room = cap.saturating_sub(ctx.resp_buf.len());
            let take = room.min(bytes.len());
            ctx.resp_buf.extend_from_slice(&bytes[..take]);
            ctx.drain_overflow += (bytes.len() - take) as u64;
        }

        /// The relay target for one request, computed from its snapshot:
        /// (URL base, SNI Host override, HTTP client). On https with an SNI
        /// override the URL host IS the SNI name and the client carries the
        /// resolve() pin to the real address (BLOCK-A) — the pinned client
        /// is cached and rebuilt only when the (upstream, sni) pair changes.
        fn relay_target(&self, ctx: &Ctx) -> (String, Option<String>, reqwest::Client) {
            let cfg = &ctx.snap.cfg;
            let sni = cfg.server.sni.trim();
            let sni = if sni.is_empty() { None } else { Some(sni) };
            let (scheme, host, port, path) = parse_upstream(&cfg.server.upstream);
            match sni_resolve_entry(&cfg.server.upstream, sni) {
                Some((name, addr)) => {
                    let base = format!("{scheme}://{name}:{port}{path}");
                    let client = {
                        let cached = self.pinned_relay.read();
                        match cached.as_ref() {
                            Some(((k_name, k_addr), client))
                                if *k_name == name && *k_addr == addr =>
                            {
                                client.clone()
                            }
                            _ => {
                                drop(cached);
                                let built = reqwest::Client::builder()
                                    // BLOCK-B (RustGate §H): ambient
                                    // HTTPS_PROXY/HTTP_PROXY env vars would
                                    // otherwise silently hijack the LLM data
                                    // path (same reasoning as the exporter's
                                    // client).
                                    .no_proxy()
                                    .resolve(&name, addr)
                                    .build()
                                    .unwrap_or_else(|_| self.http.clone());
                                *self.pinned_relay.write() =
                                    Some(((name.clone(), addr), built.clone()));
                                built
                            }
                        }
                    };
                    (base, Some(name), client)
                }
                None => {
                    let base = format!("{scheme}://{host}:{port}{path}");
                    (base, sni.map(str::to_string), self.http.clone())
                }
            }
        }

        /// Owned request relay (v0.3.6): forwards one LLM API request to
        /// the upstream and streams the response back, owning both
        /// directions so the client-disconnect policy is enforceable.
        ///
        /// Downstream liveness: a failed downstream write marks the client
        /// dead. With drain_on_cancel the upstream stream keeps being
        /// consumed to its natural end (within `drain_timeout`); otherwise
        /// the relay returns immediately and the upstream response object
        /// is dropped, closing the connection. Forwarding stops at client
        /// death in both modes (丢弃转发); drain capture is bounded — raw
        /// bytes stop at the capture cap while the incremental SSE
        /// accumulator keeps final_output/usage complete (不无界缓冲).
        ///
        /// The turn record itself is finalized by logging() from ctx, the
        /// same pipeline as the pingora-pumped paths.
        async fn relay(
            &self,
            session: &mut Session,
            ctx: &mut Ctx,
            d: &'static atg_protocol::ProtocolDescriptor,
        ) {
            // --- request side ---
            // NIT ② trade-off (RustGate §H): the request body is fully
            // buffered before the upstream call. Streaming the upload would
            // couple the upstream request start to downstream read pacing
            // and complicate death handling mid-send; at LLM request sizes
            // (JSON prompts, bounded by the capture cap at record time) the
            // buffer cost is negligible. Revisit only if large uploads
            // become a traced workload.
            loop {
                match session.downstream_session.read_request_body().await {
                    Ok(Some(b)) => ctx.req_buf.extend_from_slice(&b),
                    Ok(None) => break,
                    Err(e) => {
                        // Client vanished mid-upload: v0.3.5 parity — the
                        // request never became a turn, so no record (the
                        // relay never ran; logging() records nothing).
                        let path = session.req_header().uri.path();
                        eprintln!("ATG: client aborted during request upload (path={path}): {e}");
                        ctx.client_dead = true;
                        return;
                    }
                }
            }
            let req_head = session.req_header();
            let method = req_head.method.clone();
            let path_and_query = req_head
                .uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| req_head.uri.path().to_string());
            // The URL base and the Host header are computed from THIS
            // request's snapshot (hot keys server.upstream / server.sni —
            // a mid-process reload redirects new requests without a
            // restart). The URL base carries the SNI name on https (BLOCK-A:
            // the pinned client's resolve() entry ties it to the real
            // address); on http it is the upstream host and this Host
            // header carries the SNI override — pump parity.
            let (url, host, client) = self.relay_target(ctx);
            let url = format!("{url}{path_and_query}");
            let mut out = client.request(method, &url);
            if let Some(sni_host) = host {
                out = out.header("host", sni_host);
            }
            for (name, value) in req_head.headers.iter() {
                if is_hop_by_hop(name) || name == http::header::CONTENT_LENGTH {
                    continue;
                }
                out = out.header(name, value);
            }
            let sent = out.body(Bytes::copy_from_slice(&ctx.req_buf)).send().await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    // fail_to_connect parity: the upstream was never
                    // reached — 502 to the client, errored minimal record.
                    let path = session.req_header().uri.path();
                    eprintln!("GATEWAY relay connect failed: path={path} error={e}");
                    ctx.relay_error = Some(format!("proxy_error: connect: {e}"));
                    session.respond_error(502).await.ok();
                    ctx.relay_done = true;
                    return;
                }
            };

            // --- response head ---
            // Upstream first byte is in (v0.3.12 observability): the
            // awaiting_upstream gauge releases and the wait stage closes —
            // the pump path latches the same boundary in
            // upstream_response_filter.
            self.mark_upstream_head(ctx);
            ctx.resp_status = resp.status().as_u16();
            if let Some(v) = resp.headers().get(http::header::CONTENT_TYPE) {
                ctx.resp_content_type = v.to_str().unwrap_or("").to_string();
            }
            let mut head = match ResponseHeader::build(ctx.resp_status, None) {
                Ok(h) => h,
                Err(e) => {
                    ctx.relay_error = Some(format!("proxy_error: response head: {e}"));
                    ctx.relay_done = true;
                    return;
                }
            };
            let has_content_length = resp.headers().contains_key(http::header::CONTENT_LENGTH);
            // NIT ① (RustGate §H): HTTP trailers are not relayed —
            // reqwest's bytes_stream exposes data frames only, no trailer
            // API. LLM API responses carry no trailers (the pump already
            // skips them for h1 downstream); recorded here as a known
            // limitation, not an oversight.
            for (name, value) in resp.headers().iter() {
                if is_hop_by_hop(name) {
                    continue;
                }
                let _ = head.insert_header(name, value);
            }
            let no_body_status = matches!(ctx.resp_status, 204 | 304)
                || session.req_header().method == http::Method::HEAD;
            // Framing parity with pingora's h1 pump: a body response with
            // neither framing header would hang the downstream writer —
            // declare chunked (h2 downstream frames by DATA messages; no
            // TE there).
            if !has_content_length && !no_body_status && !session.downstream_session.is_http2() {
                let _ = head.insert_header("transfer-encoding", "chunked");
            }
            let head_end = no_body_status;
            if let Err(e) = session
                .write_response_header(Box::new(head), head_end)
                .await
            {
                // Client died before the headers landed; the drain
                // decision applies from here on.
                let path = session.req_header().uri.path();
                eprintln!("ATG: client cancelled mid-turn (path={path}) — head write failed: {e}");
                ctx.client_dead = true;
                if !ctx.snap.cfg.drain.on_cancel {
                    ctx.relay_done = true;
                    return;
                }
            }

            // --- response body pump ---
            let mut stream = resp.bytes_stream();
            let mut drain_deadline: Option<tokio::time::Instant> = None;
            let mut upstream_failed = false;
            let mut end_seen = no_body_status;
            while !end_seen {
                // The pump watches BOTH sides: upstream chunks and (while
                // the client is believed alive) the downstream read half —
                // `idle()` errors the moment the client disconnects, even
                // when the upstream is silent and no write would notice.
                // This is the same liveness watch pingora's pump runs.
                let next = if ctx.client_dead {
                    // Draining: the upstream read is bounded by the drain
                    // deadline — a stream that will not finish inside the
                    // window is abandoned.
                    match tokio::time::timeout_at(
                        drain_deadline.unwrap_or_else(tokio::time::Instant::now),
                        stream.next(),
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(_elapsed) => {
                            ctx.drain_timed_out = true;
                            break;
                        }
                    }
                } else {
                    tokio::select! {
                        chunk = stream.next() => chunk,
                        probe = session.downstream_session.read_body_or_idle(true) => {
                            match probe {
                                // EOF/RST from the client mid-response:
                                // the disconnect fact, caught even when
                                // the upstream is silent (no write is
                                // pending to notice it).
                                Err(_) => {
                                    let path = session.req_header().uri.path();
                                    eprintln!(
                                        "ATG: client cancelled mid-turn (path={path}) — downstream gone"
                                    );
                                    ctx.client_dead = true;
                                    if !ctx.snap.cfg.drain.on_cancel {
                                        break; // abort: dropping `resp` closes the upstream
                                    }
                                    drain_deadline = Some(self.begin_drain(ctx, d));
                                    continue;
                                }
                                // NIT ④ (RustGate §H, pingora source): the
                                // probe NEVER returns Ok in this mode —
                                // idle() maps every outcome to a session
                                // teardown error: clean FIN = ConnectionClosed,
                                // RST/read error = ReadError ("during HTTP
                                // idle state"), bytes arriving after the body
                                // ended = ConnectError. The pump errors the
                                // session on all three (its downstream arm);
                                // this arm is a defensive keep-alive only.
                                Ok(_) => continue,
                            }
                        }
                    }
                };
                let Some(chunk) = next else {
                    end_seen = true;
                    break;
                };
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        // Upstream interrupted mid-stream: ProxyError class
                        // (v0.3.5 taxonomy) — the turn is errored, the
                        // downstream connection is left truncated.
                        ctx.relay_error = Some(format!("proxy_error: upstream read: {e}"));
                        upstream_failed = true;
                        break;
                    }
                };
                if ctx.first_output_ns.is_none() && !bytes.is_empty() {
                    ctx.first_output_ns = Some(now_ns());
                }
                if !ctx.client_dead {
                    ctx.resp_buf.extend_from_slice(&bytes);
                    if let Err(e) = session.write_response_body(Some(bytes), false).await {
                        let path = session.req_header().uri.path();
                        eprintln!(
                            "ATG: client cancelled mid-turn (path={path}) — downstream write failed: {e}"
                        );
                        ctx.client_dead = true;
                        if !ctx.snap.cfg.drain.on_cancel {
                            break; // abort: dropping `resp` closes the upstream
                        }
                        drain_deadline = Some(self.begin_drain(ctx, d));
                    }
                } else if ctx.snap.cfg.drain.on_cancel {
                    // Draining: forward nothing, capture bounded, parse
                    // incrementally so final_output/usage survive the cap.
                    self.drain_capture(ctx, &bytes);
                    if ctx.drain_acc.is_none() && unpack::looks_like_sse(&ctx.resp_content_type) {
                        ctx.drain_acc = Some(crate::engine::SseAccum::default());
                    }
                    if let Some(acc) = ctx.drain_acc.as_mut() {
                        crate::engine::drain_feed(acc, d, &mut ctx.drain_tail, &bytes);
                    }
                } else {
                    // Drain disabled with a dead client is unreachable
                    // (the live branch breaks on client death) — defensive.
                    break;
                }
            }
            if end_seen && !upstream_failed {
                ctx.response_complete = true;
            }
            // Response end. The terminating write failing after full
            // delivery is post-response teardown (v0.3.5 IdleNoise class):
            // not a cancellation — the turn stays clean.
            if !ctx.client_dead && !upstream_failed {
                let _ = session.write_response_body(None, true).await;
            }
            // Relay end: close the delivery stage (pump parity — the pump
            // path latches this in response_body_filter's end arm).
            if ctx.body_end_ns.is_none() {
                ctx.body_end_ns = Some(now_ns());
            }
            ctx.relay_done = true;
        }
    }

    /// Headers that never enter the evidence map (issue #32 ruling: keep ALL
    /// request headers except a blacklist). Names are lowercase — the
    /// `HeaderMap` normalises — and this list is the security boundary: a
    /// header that carries a credential belongs here.
    const HDR_EVIDENCE_DENY: &[&str] = &[
        // credential carriers
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api-key",
        "x-goog-api-key",
        // cookies
        "cookie",
        "set-cookie",
        // hop-by-hop transport headers (README: transport noise is stripped)
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    /// Defence in depth for client-invented credential headers: a name that
    /// contains any of these markers is dropped even when it is not listed
    /// above (`x-auth-token`, `x-custom-secret`, ...). Documented false
    /// positives: rate-limit headers whose names mention tokens.
    const HDR_EVIDENCE_DENY_MARKERS: &[&str] = &[
        "authorization",
        "cookie",
        "secret",
        "password",
        "credential",
        "signature",
        "token",
        "bearer",
        "api-key",
        "apikey",
        "access-key",
        "private-key",
    ];
    /// Header names longer than this are dropped: name bytes are
    /// client-controlled and would otherwise eat the budget.
    const HDR_NAME_CAP: usize = 64;
    /// Per-value cap, in chars on a char boundary — same rule as `client_ua`
    /// (the attribute is unbounded otherwise: header values are client input).
    const HDR_VALUE_CAP: usize = 256;
    /// Per-turn entry cap — a client cannot inflate the metadata map.
    const HDR_EVIDENCE_MAX_ENTRIES: usize = 32;
    /// Per-turn budget across the evidence entries (name + value bytes).
    const HDR_EVIDENCE_BUDGET: usize = 4096;

    /// Salted API-credential fingerprint: sha256(salt || key), first 16
    /// hex chars. The salt comes from the config file (`api_key.salt`,
    /// passed explicitly — rotating it invalidates cross-version
    /// correlation but never exposes the key). The raw key is dropped
    /// immediately — no log, record or export path ever sees the plaintext.
    pub fn api_key_fp(key: &str, salt: &str) -> String {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update(key.as_bytes());
        hex::encode(hasher.finalize())[..16].to_string()
    }

    /// Cap a header value at `max` chars without splitting a codepoint
    /// (`char_indices().nth` gives the byte offset of the (max+1)-th char).
    fn truncate_header_value(value: &str, max: usize) -> &str {
        match value.char_indices().nth(max) {
            Some((i, _)) => &value[..i],
            None => value,
        }
    }

    /// Whether a request header is barred from the evidence map.
    fn hdr_evidence_denied(name: &str) -> bool {
        HDR_EVIDENCE_DENY.contains(&name)
            || HDR_EVIDENCE_DENY_MARKERS
                .iter()
                .any(|marker| name.contains(*marker))
    }

    /// Collect one turn's header evidence: every request header except the
    /// deny list, values trimmed and capped at `HDR_VALUE_CAP` chars,
    /// over-long names skipped, bounded by `HDR_EVIDENCE_MAX_ENTRIES` and
    /// `HDR_EVIDENCE_BUDGET`. Returns the kept entries and how many headers
    /// the bounds dropped (callers count that in health; deny-list hits are
    /// policy, not overflow, and are not counted).
    fn collect_hdr_evidence(headers: &http::HeaderMap) -> (Vec<(String, String)>, u32) {
        let mut kept: Vec<(String, String)> = Vec::with_capacity(16);
        let mut used = 0usize;
        let mut dropped = 0u32;
        for (name, raw) in headers {
            let name = name.as_str();
            if hdr_evidence_denied(name) {
                continue;
            }
            if name.len() > HDR_NAME_CAP {
                dropped = dropped.saturating_add(1);
                continue;
            }
            // Non-UTF-8 values cannot be carried as JSON strings; skip them
            // (not an overflow, so not counted).
            let Ok(raw) = raw.to_str() else {
                continue;
            };
            let value = truncate_header_value(raw.trim(), HDR_VALUE_CAP);
            if value.is_empty() {
                continue;
            }
            let cost = name.len().saturating_add(value.len());
            if kept.len() >= HDR_EVIDENCE_MAX_ENTRIES
                || used.saturating_add(cost) > HDR_EVIDENCE_BUDGET
            {
                dropped = dropped.saturating_add(1);
                continue;
            }
            used = used.saturating_add(cost);
            kept.push((name.to_string(), value.to_string()));
        }
        (kept, dropped)
    }

    /// Extract the request's API credential and fingerprint it:
    /// anthropic.messages -> x-api-key header; openai.* -> Authorization
    /// Bearer. None when the request carried no credential (internal
    /// probes) — no error, no field.
    fn request_api_key_fp(
        protocol: &str,
        header_get: &dyn Fn(&str) -> Option<String>,
        salt: &str,
    ) -> String {
        let key = if protocol == "anthropic.messages" {
            header_get("x-api-key")
        } else {
            header_get("authorization").and_then(|v| {
                v.strip_prefix("Bearer ")
                    .map(str::to_string)
                    // Non-Bearer schemes are not API credentials here.
                    .filter(|k| !k.is_empty())
            })
        };
        match key {
            Some(k) if !k.trim().is_empty() => api_key_fp(k.trim(), salt),
            _ => String::new(),
        }
    }

    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Hop-by-hop headers (RFC 9110 §7.6.1) never forwarded in either
    /// direction by the owned relay; framing headers are re-derived per
    /// hop. Content-length is handled separately (request side: the client
    /// re-frames; response side: forwarded verbatim when present).
    fn is_hop_by_hop(name: &http::HeaderName) -> bool {
        matches!(
            name.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-connection"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "host"
                | "proxy-authorization"
                | "proxy-authenticate"
        )
    }

    /// Fail-open identification (v0.3.7): path identification is the FIRST
    /// step of every request — a panic there took the request down with
    /// zero delivery (the loose-detect incident). This wrapper demotes any
    /// panic to None: an unknown protocol is transparently forwarded, the
    /// turn is simply not recorded. Fail-open as a mechanism, not a hope.
    fn detect_path_fail_open(path: &str) -> Option<atg_protocol::PathMatch> {
        std::panic::catch_unwind(|| atg_protocol::ProtocolDescriptor::detect_path(path))
            .unwrap_or_else(|_| {
                eprintln!("ATG: path identification panicked (path={path:?}) — failing open");
                None
            })
    }

    /// Effective capture mode as self-attested by health, metrics and the
    /// startup line. The production build can only ever report `full`.
    fn trace_mode_label(trace_off: bool) -> &'static str {
        if trace_off {
            "off"
        } else {
            "full"
        }
    }

    /// Build variant (v0.3.12, ATG issue #2): `bench` iff the bench feature
    /// is compiled in, `prod` otherwise. Compile-time by construction — a
    /// bench binary cannot claim to be production, whatever its environment
    /// says.
    #[cfg(feature = "bench-trace-mode")]
    fn variant_label() -> &'static str {
        "bench"
    }

    /// Build variant, production arm.
    #[cfg(not(feature = "bench-trace-mode"))]
    fn variant_label() -> &'static str {
        "prod"
    }

    /// BLOCK-A (RustGate §H): the relay's TLS SNI is the URL host — reqwest
    /// has no SNI override. With ATG_SNI set on an https upstream, the
    /// relay's URLs carry the SNI NAME and this entry pins that name to the
    /// real upstream address (connect to the IP, SNI = the override) —
    /// exactly what upstream_peer's HttpPeer did for the pump. Returns the
    /// (name, sockaddr) for ClientBuilder::resolve.
    fn sni_resolve_entry(
        upstream: &str,
        sni: Option<&str>,
    ) -> Option<(String, std::net::SocketAddr)> {
        let (scheme, host, port, _) = parse_upstream(upstream);
        if scheme != "https" {
            return None;
        }
        let sni_host = sni?;
        if sni_host == host {
            return None; // normal DNS, no pinning needed
        }
        // Bracketed form for IPv6 literals — to_socket_addrs requires it.
        let hostport = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let addr = hostport.to_socket_addrs().ok()?.next()?;
        Some((sni_host.to_string(), addr))
    }

    pub struct Ctx {
        /// The request's config snapshot — taken ONCE in `new_ctx` and read
        /// for the request's whole lifetime (hot values never straddle a
        /// reload inside one request).
        pub snap: Arc<crate::config_store::Snapshot>,
        pub req_buf: Vec<u8>,
        pub resp_buf: Vec<u8>,
        pub resp_content_type: String,
        pub ws_client_parser: atg_protocol::openai::live::WsFrameParser,
        pub ws_server_parser: atg_protocol::openai::live::WsFrameParser,
        pub ws_turn: atg_protocol::openai::live::WsTurnState,
        /// Turn timing (unix nanoseconds). start set at request start,
        /// end set when the record is finalized.
        pub start_ns: u64,
        pub end_ns: u64,
        /// P1-8: first output byte on the wire (first SSE body chunk /
        /// first WS server frame of the turn) — the truthful
        /// completion-start moment, reset per turn.
        pub first_output_ns: Option<u64>,
        /// Upstream response headers observed (pump: upstream_response_filter;
        /// relay: right after the upstream response arrives) — the boundary
        /// between "waiting" and "streaming" for the stage histograms and the
        /// awaiting_upstream gauge latch.
        pub upstream_head_ns: Option<u64>,
        /// Response body end observed (pump: response_body_filter end; relay:
        /// end of the relay pump) — closes the delivery stage and starts the
        /// finalize stage.
        pub body_end_ns: Option<u64>,
        /// Upstream response status (0 = no upstream response arrived —
        /// proxy-level failure); captured at upstream_response_filter.
        pub resp_status: u16,
        /// The response body stream ran to its end (end_of_stream reached
        /// in response_body_filter) — the "fully delivered" gate for the
        /// idle-teardown classifier. resp_status alone only proves the
        /// HEADERS arrived; a mid-body disconnect must not classify as
        /// idle (BLOCK: truncated responses would launder into clean turns).
        pub response_complete: bool,
        /// Owned-relay state (v0.3.6). The client disconnected mid-turn —
        /// detected by the relay on a failed downstream write (or aborted
        /// request upload). The turn records cancelled=true, never an error.
        pub client_dead: bool,
        /// The relay ran to completion (one way or another) — lets logging
        /// distinguish a drained turn from an aborted upload.
        pub relay_done: bool,
        /// Drain window elapsed before the upstream stream finished.
        pub drain_timed_out: bool,
        /// Incremental SSE parse state during drain: the accumulator holds
        /// the COMPLETE semantic content (text/usage/tools) even when the
        /// raw capture hits its cap; `drain_tail` retains the raw bytes of
        /// the frame currently being received. Bounded memory, unclipped
        /// final_output/usage (spec: 丢弃转发但不无界缓冲).
        pub drain_acc: Option<crate::engine::SseAccum>,
        pub drain_tail: Vec<u8>,
        /// Raw drain bytes dropped beyond the capture cap (for the honest
        /// truncation marker at record time).
        pub drain_overflow: u64,
        /// Proxy/upstream failure observed by the relay (marker text for
        /// the record's error field; relayed requests carry no pingora
        /// session error for logging() to classify).
        pub relay_error: Option<String>,
    }

    #[async_trait]
    impl ProxyHttp for Gateway {
        type CTX = Ctx;

        fn new_ctx(&self) -> Self::CTX {
            // v0.3.12 observability: pingora calls new_ctx exactly once per
            // request, and logging exactly once at its end — the bracket the
            // inflight/awaiting gauges rely on.
            self.enter_request();
            // THE config read for this request: one snapshot, carried in
            // ctx and read by every hook below (hot values are consistent
            // within the request even across a concurrent reload).
            let snap = self.config_store.snapshot();
            let ws_cap = snap.cfg.server.max_ws_frame_payload.min(usize::MAX as u64) as usize;
            Ctx {
                snap,
                req_buf: Vec::new(),
                resp_buf: Vec::new(),
                resp_content_type: String::new(),
                ws_client_parser: atg_protocol::openai::live::WsFrameParser::with_max_payload(
                    true, ws_cap,
                ),
                ws_server_parser: atg_protocol::openai::live::WsFrameParser::with_max_payload(
                    false, ws_cap,
                ),
                ws_turn: atg_protocol::openai::live::WsTurnState::default(),
                start_ns: now_ns(),
                end_ns: 0,
                first_output_ns: None,
                upstream_head_ns: None,
                body_end_ns: None,
                resp_status: 0,
                response_complete: false,
                client_dead: false,
                relay_done: false,
                drain_timed_out: false,
                drain_acc: None,
                drain_tail: Vec::new(),
                drain_overflow: 0,
                relay_error: None,
            }
        }

        async fn upstream_peer(
            &self,
            _session: &mut Session,
            ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            // Per-request snapshot (hot keys server.upstream / server.sni).
            let cfg = &ctx.snap.cfg;
            let (scheme, host, port, _) = parse_upstream(&cfg.server.upstream);
            let tls = scheme == "https";
            // server.sni overrides SNI so an upstream given as a bare IP can
            // still complete TLS with the correct hostname; empty = the
            // upstream host itself.
            let sni = match cfg.server.sni.trim() {
                "" => host.clone(),
                s => s.to_string(),
            };
            let addr = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            Ok(Box::new(HttpPeer::new(addr, tls, sni)))
        }

        // Rewrite the Host header so the real upstream routes the request
        // correctly. Uses server.sni when set, else the upstream host.
        async fn upstream_request_filter(
            &self,
            _session: &mut Session,
            upstream_request: &mut pingora::http::RequestHeader,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
            let cfg = &ctx.snap.cfg;
            let host = match cfg.server.sni.trim() {
                "" => {
                    // The upstream authority AS WRITTEN (scheme stripped,
                    // base path dropped): an implicit port stays implicit
                    // and an explicit one stays explicit — byte-identical to
                    // the pre-v0.4 Host header for every deployed upstream.
                    cfg.server
                        .upstream
                        .trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .split('/')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                }
                s => s.to_string(),
            };
            let _ = upstream_request.insert_header(http::header::HOST, host);
            Ok(())
        }

        // Control endpoint: dump collected turn records as JSON.
        async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
            if session.req_header().uri.path() == "/__atg/metrics" {
                // Prometheus text exposition (v0.3.12, ATG issue #2). Plain
                // text, no dependencies; safe to scrape every few seconds
                // (reads relaxed atomics only).
                let body = self.render_metrics();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "text/plain; version=0.0.4; charset=utf-8")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            if session.req_header().uri.path() == "/__atg/records" {
                // Pagination (v0.3.9): ?limit=N&offset=M windows the NEWEST
                // end — endpoint access no longer clones the whole store
                // (a full pull used to duplicate every captured byte).
                let (mut limit, mut offset) = (0usize, 0usize);
                if let Some(q) = session.req_header().uri.query() {
                    for pair in q.split('&') {
                        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                        match k {
                            "limit" => limit = v.parse().unwrap_or(0),
                            "offset" => offset = v.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                let records = self.store.snapshot_bounded(limit, offset);
                let body = serde_json::to_vec(&records).unwrap_or_default();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "application/json")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            if session.req_header().uri.path() == "/__atg/health" {
                let snap = self.config_store.snapshot();
                let (exported, failed, dropped, panicked) = self.exporter.health.snapshot();
                let (store_wait_ns, store_hold_ns) = self.store.lock_ns_totals();
                let failed_frames = self
                    .failed_frames
                    .load(std::sync::atomic::Ordering::Relaxed);
                let turns_total = self.turns_total.load(std::sync::atomic::Ordering::Relaxed);
                let loose_path_matches = self
                    .loose_path_matches
                    .load(std::sync::atomic::Ordering::Relaxed);
                let turns_with_session = self
                    .turns_with_session
                    .load(std::sync::atomic::Ordering::Relaxed);
                let turns_with_harness = self
                    .turns_with_harness
                    .load(std::sync::atomic::Ordering::Relaxed);
                let body = serde_json::json!({
                    // Self-attestation (v0.3.12, ATG issue #2): a measurement
                    // or incident record names the binary, the build variant
                    // and the effective capture mode it ran against.
                    "version": env!("CARGO_PKG_VERSION"),
                    "variant": variant_label(),
                    "trace_mode": trace_mode_label(self.trace_off),
                    "exported": exported,
                    "failed": failed,
                    "dropped": dropped,
                    "panicked": panicked,
                    "store_dropped": self.store.dropped(),
                    "export_queue_depth": self.exporter.queue_depth(),
                    // Export flush pool (ATG #13): the gauge the drop
                    // accounting is read against (a batch in flight is
                    // neither exported nor dropped yet), plus the two knob
                    // values this instance actually ran with — a throughput
                    // number is only attributable to a setting if the
                    // instance self-attests it.
                    "export_inflight": self.exporter.health.inflight(),
                    "export_max_inflight": self.exporter.max_inflight(),
                    "export_workers": self.exporter.workers(),
                    // Prefix-stitch state (ATG#5 entry ticket): the table size
                    // is what scales the stitcher's O(table) sweep cost, so a
                    // deployment must be able to read it without a debugger —
                    // it decides whether that serial point is a real
                    // bottleneck at this instance's load.
                    "stitch_entries": self.stitcher.entries(),
                    "stitch_capacity": self.stitcher.capacity(),
                    "stitch_expired_total": self.stitcher.expired_total(),
                    "stitch_evicted_total": self.stitcher.evicted_total(),
                    // Lock attribution (ATG#5): cumulative nanoseconds spent
                    // waiting for / holding the two per-request locks. Divide
                    // by requests_total for the per-request share, and compare
                    // across thread counts to see how much of the
                    // multi-threading cost these serial sections eat.
                    "stitch_wait_ns_total": self.stitcher.lock_wait_ns_total(),
                    "stitch_hold_ns_total": self.stitcher.lock_hold_ns_total(),
                    "store_wait_ns_total": store_wait_ns,
                    "store_hold_ns_total": store_hold_ns,
                    "failed_frames": failed_frames,
                    "turns_total": turns_total,
                    "loose_path_matches": loose_path_matches,
                    "turns_with_session": turns_with_session,
                    "turns_with_harness": turns_with_harness,
                    // Header evidence (issue #32): entries dropped by the
                    // per-turn byte budget — overflow is countable.
                    "hdr_evidence_dropped": self
                        .hdr_evidence_dropped
                        .load(std::sync::atomic::Ordering::Relaxed),
                    // Queue/backpressure (v0.3.12): requests inside the
                    // gateway, the subset still waiting on the upstream, the
                    // monotonic max, and the effective worker-thread count.
                    "inflight": self.inflight.load(std::sync::atomic::Ordering::Relaxed),
                    "inflight_high_water": self
                        .inflight_high_water
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "awaiting_upstream": self
                        .awaiting_upstream
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "worker_threads": self.worker_threads,
                    // Config provenance (v0.4.0, ADDITIVE — only new keys):
                    // which file is live, what generation, what still needs a
                    // restart, and how many reloads failed.
                    "config_path": snap.path.display().to_string(),
                    "config_sha256": hex::encode(snap.sha256),
                    "config_generation": snap.generation,
                    "config_loaded_at_ms": u64::try_from(snap.loaded_at_ms).unwrap_or(u64::MAX),
                    "config_restart_required": snap.restart_required,
                    "config_reload_failures": self.config_store.reload_failures(),
                })
                .to_string();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "application/json")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            if session.req_header().uri.path() == "/__atg/config" {
                // Read-only config view (v0.4.0): file provenance, the
                // effective values (salt redacted to "***"), and the
                // structural RUNNING values vs the file.
                let snap = self.config_store.snapshot();
                let body = serde_json::json!({
                    "config": {
                        "path": snap.path.display().to_string(),
                        "sha256": hex::encode(snap.sha256),
                        "generation": snap.generation,
                        "loaded_at_ms": u64::try_from(snap.loaded_at_ms).unwrap_or(u64::MAX),
                        "restart_required": snap.restart_required,
                        "reload_failures": self.config_store.reload_failures(),
                    },
                    "effective": crate::config::redacted_json(&snap.cfg),
                    "structural": {
                        "server.listen": self.listen,
                        "server.worker_threads": self.worker_threads,
                        "export.workers": self.exporter.workers(),
                        "trace.mode": trace_mode_label(self.trace_off),
                    }
                })
                .to_string();
                let mut resp = ResponseHeader::build(200, None)?;
                resp.insert_header("content-type", "application/json")?;
                resp.insert_header("content-length", body.len().to_string())?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from(body)), true)
                    .await?;
                return Ok(true);
            }
            // Owned relay (v0.3.6, BLOCK-C gated): ONLY drain_on_cancel=true
            // routes LLM API requests through the gateway's own relay — the
            // pump couples downstream liveness to upstream consumption
            // (first failed downstream read/write aborts the session and
            // drops the upstream connection), which makes the disconnect
            // policy unenforceable in the ProxyHttp hooks. The default
            // (false) keeps the pingora pump for every request — zero
            // regression; its abort-on-disconnect IS the drain-off policy.
            // WebSocket upgrades and unknown paths keep the pump in both
            // modes.
            let path = session.req_header().uri.path();
            let is_ws_upgrade = session
                .req_header()
                .headers
                .get(http::header::UPGRADE)
                .is_some()
                || session.req_header().method == http::Method::CONNECT;
            if ctx.snap.cfg.drain.on_cancel && !is_ws_upgrade {
                if let Some(matched) = detect_path_fail_open(path) {
                    self.relay(session, ctx, matched.descriptor).await;
                    return Ok(true);
                }
            }
            Ok(false)
        }

        async fn request_body_filter(
            &self,
            session: &mut Session,
            body: &mut Option<Bytes>,
            _end: bool,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
            if session.was_upgraded() {
                if let Some(b) = body {
                    // Fail-open (v0.3.8): a WS parse panic must not kill the
                    // request — the parse state resets and the stream keeps
                    // forwarding transparently.
                    let parse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        for payload in ctx.ws_client_parser.push(b) {
                            ctx.ws_turn.apply_client_frame(&payload);
                        }
                    }));
                    if parse.is_err() {
                        eprintln!(
                            "ATG: ws client frame parse panicked — parse state reset, forwarding continues"
                        );
                        ctx.ws_client_parser =
                            atg_protocol::openai::live::WsFrameParser::with_max_payload(
                                true,
                                ctx.snap.cfg.server.max_ws_frame_payload as usize,
                            );
                        ctx.ws_turn = atg_protocol::openai::live::WsTurnState::default();
                    }
                }
            } else if let Some(b) = body {
                // Trace-mode off (v0.3.11): no request capture — the pump
                // forwards the chunk upstream regardless (pingora sends the
                // filter's untouched body).
                if !self.trace_off {
                    ctx.req_buf.extend_from_slice(b);
                }
            }
            Ok(())
        }

        async fn upstream_response_filter(
            &self,
            _session: &mut Session,
            resp: &mut pingora::http::ResponseHeader,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
            // Stage boundary (v0.3.12): upstream headers in.
            self.mark_upstream_head(ctx);
            ctx.resp_status = resp.status.as_u16();
            if let Some(v) = resp.headers.get(http::header::CONTENT_TYPE) {
                ctx.resp_content_type = v.to_str().unwrap_or("").to_string();
            }
            Ok(())
        }

        fn response_body_filter(
            &self,
            session: &mut Session,
            body: &mut Option<Bytes>,
            end: bool,
            ctx: &mut Self::CTX,
        ) -> Result<Option<std::time::Duration>> {
            if end {
                ctx.response_complete = true;
                // Stage boundary (v0.3.12): response body end.
                if ctx.body_end_ns.is_none() {
                    ctx.body_end_ns = Some(now_ns());
                }
            }
            if session.was_upgraded() {
                if let Some(b) = body {
                    // Fail-open (v0.3.8): same degrade contract as the
                    // client arm — the stream keeps flowing, a partial
                    // turn may be lost instead of the request.
                    let parse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        for payload in ctx.ws_server_parser.push(b) {
                            if ctx.ws_turn.active() && ctx.first_output_ns.is_none() {
                                ctx.first_output_ns = Some(now_ns());
                            }
                            if let Some(mut record) = ctx.ws_turn.apply_server_frame(&payload) {
                                ctx.end_ns = now_ns();
                                record.start_ns = ctx.start_ns;
                                record.end_ns = ctx.end_ns;
                                record.completion_start_ns = ctx.first_output_ns.take();
                                record.trace_tag = ctx.snap.cfg.trace.tag.clone();
                                // WS turns get the same header evidence as
                                // HTTP turns (the upgrade request's headers
                                // are still on the session).
                                let (evidence, dropped) =
                                    collect_hdr_evidence(&session.req_header().headers);
                                if dropped > 0 {
                                    self.hdr_evidence_dropped.fetch_add(
                                        u64::from(dropped),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                                record.hdr_evidence = evidence;
                                self.push_record(record);
                            }
                        }
                    }));
                    if parse.is_err() {
                        eprintln!(
                            "ATG: ws server frame parse panicked — parse state reset, forwarding continues"
                        );
                        ctx.ws_server_parser =
                            atg_protocol::openai::live::WsFrameParser::with_max_payload(
                                false,
                                ctx.snap.cfg.server.max_ws_frame_payload as usize,
                            );
                        ctx.ws_turn = atg_protocol::openai::live::WsTurnState::default();
                    }
                }
            } else if let Some(b) = body {
                if ctx.resp_buf.is_empty() && !b.is_empty() {
                    ctx.first_output_ns = Some(now_ns());
                }
                // Trace-mode off (v0.3.11): no response capture — timing
                // observability (first_output) still rides this hook.
                if !self.trace_off {
                    ctx.resp_buf.extend_from_slice(b);
                }
            }
            Ok(None)
        }

        async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
            let path = session.req_header().uri.path();
            let matched = detect_path_fail_open(path);
            // v0.3.12 observability: the gauges release for EVERY request
            // (the new_ctx bracket, pingora guarantees this pair); the stage
            // histograms take observations only for identified protocol
            // paths — self-probes (/__atg/*) and unknown paths would skew
            // the LLM-turn latency picture.
            self.exit_request(ctx, matched.is_some());
            let Some(matched) = matched else {
                return;
            };
            let protocol = matched.descriptor.name;
            // Loose (endpoint-variant) match: count it, log the first one
            // (path + protocol — base-URL misconfiguration debugging), and
            // record ONLY on a 2xx upstream response. Exact matches keep
            // their existing semantics: every turn records, errors carry
            // the error marker.
            let loose = matched.loose;
            let record_allowed = if loose {
                let total = self
                    .loose_path_matches
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if total == 0 {
                    eprintln!("ATG: loose path match path={path} protocol={protocol}");
                }
                (200..300).contains(&ctx.resp_status)
            } else {
                true
            };
            if loose && !record_allowed {
                return;
            }
            // G3: HTTP/proxy-level failure marker — protocol terminal error
            // frames (error_marker) only cover in-stream failures; a 4xx/5xx
            // JSON body or a proxy error previously exported as a
            // successful turn. Protocol-level markers win when both exist.
            // Three-way session-error classification (see
            // classify_session_error): idle noise marks nothing; a client
            // cancellation marks cancelled=true (no error — partial
            // content records normally, reconciliation material);
            // everything else keeps the proxy_error marker. The owned
            // relay sets client_dead directly (a failed downstream write)
            // and carries its own proxy_error markers in relay_error —
            // relayed requests surface here with no pingora session error.
            let mut client_cancelled = ctx.client_dead;
            let http_error = if let Some(e) = e {
                let err_text = format!("{e:?}");
                let downstream_sourced = *e.esource() == pingora::ErrorSource::Downstream;
                match classify_session_error(
                    &err_text,
                    downstream_sourced,
                    ctx.resp_status,
                    ctx.response_complete,
                ) {
                    SessionErrorClass::IdleNoise => {
                        eprintln!(
                            "ATG: idle teardown after response (path={path}) — not a failure"
                        );
                        None
                    }
                    SessionErrorClass::ClientCancelled => {
                        eprintln!("ATG: client cancelled mid-turn (path={path})");
                        client_cancelled = true;
                        None
                    }
                    SessionErrorClass::ProxyError => Some(format!("proxy_error: {err_text}")),
                }
            } else if let Some(marker) = ctx.relay_error.clone() {
                // Relay-owned proxy/upstream failure outranks a bare
                // status marker (parity: the pump's HttpTask::Failed path
                // also reports proxy_error, not the status).
                Some(marker)
            } else if ctx.resp_status >= 400 {
                Some(format!("http_status: {}", ctx.resp_status))
            } else {
                None
            };
            // Trace-mode off (v0.3.11): minimal shell records — timing,
            // error and cancel facts only. No request/response parsing, no
            // harness/session extraction: that parse cost is exactly what
            // the mode removes from the hot path.
            if self.trace_off {
                ctx.end_ns = now_ns();
                self.turns_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut shell = atg_model::TurnRecord {
                    protocol: protocol.to_string(),
                    error: http_error,
                    cancelled: client_cancelled,
                    drain_timed_out: ctx.drain_timed_out,
                    start_ns: ctx.start_ns,
                    end_ns: ctx.end_ns,
                    completion_start_ns: ctx.first_output_ns,
                    ..Default::default()
                };
                shell.trace_tag = ctx.snap.cfg.trace.tag.clone();
                self.push_record(shell);
                return;
            }
            // Fail-open record assembly (v0.3.8): everything below is
            // observability — the response is already delivered, so a panic
            // here must degrade to "no turn recorded", never kill the
            // request task.
            let header_get = |name: &str| -> Option<String> {
                session
                    .req_header()
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Single parse of req_buf — shared with every extractor (C14).
                let parsed_req: Option<serde_json::Value> = unpack::parse_body(&ctx.req_buf);
                // Harness attribution (orthogonal to session extraction; the
                // user-agent is the canary-path signal — the main OTLP path
                // records no UA).
                let ua = header_get("user-agent");
                let hfacts = atg_harness::identify(
                    protocol,
                    parsed_req.as_ref(),
                    ua.as_deref(),
                    &header_get,
                );
                // Two-tier (v0.3.2): identity label for harness metadata/tags;
                // the matched session-carrying dialect rides its own field.
                let harness = hfacts.harness_label();
                let dialect = hfacts.dialect.to_string();
                // Attribution-evidence audit: record the UA this gateway
                // actually saw (production misattribution triage) — capped at
                // 256 bytes on a char boundary (unbounded header attribute).
                let client_ua = ua
                    .as_deref()
                    .map(|u| truncate_header_value(u, HDR_VALUE_CAP).to_string())
                    .unwrap_or_default();
                // Header evidence (issue #32): every request header except
                // the deny list, with the values the client actually sent.
                // Values are trimmed and capped exactly like the UA; the
                // entry/byte bounds drop the overflow (counted) so a client
                // cannot inflate the record.
                let (hdr_evidence, hdr_dropped) =
                    collect_hdr_evidence(&session.req_header().headers);
                if hdr_dropped > 0 {
                    self.hdr_evidence_dropped
                        .fetch_add(u64::from(hdr_dropped), std::sync::atomic::Ordering::Relaxed);
                }
                // Salted API-credential fingerprint (correlation without the
                // key; plaintext never enters any downstream path).
                let api_key_fp =
                    request_api_key_fp(protocol, &header_get, ctx.snap.cfg.api_key.salt());
                // Request-side facts: ONE descriptor lookup + ONE pass over
                // the parsed body (F6 single entry; the old scattered
                // detect_by_name calls and the messages re-parse are gone).
                let tf = parsed_req
                    .as_ref()
                    .and_then(|req| unpack::turn_facts(protocol, req, &header_get, &hfacts));
                let mut session_id = tf
                    .as_ref()
                    .and_then(|f| f.session_id.clone())
                    .unwrap_or_default();
                // Provenance for the trace (issue #32): which source supplied
                // the id — a header name, a body path, or the stitcher. Empty
                // without a session (never invented).
                let mut session_source = tf
                    .as_ref()
                    .map(|f| f.session_source.clone())
                    .unwrap_or_default();
                let mut session_synthetic = false;
                // F3 tightening (user ruling): the stitcher runs ONLY for
                // protocols with session semantics (anthropic/responses — chat
                // SDK traffic is stateless single-shot, force-stitching is
                // noise) and only mints a session when the replayed chain has
                // >=2 messages (a single message cannot evidence continuity).
                let stitch_eligible = tf.as_ref().is_some_and(|f| {
                    f.stitch_eligible && f.messages.as_ref().is_some_and(|m| m.len() >= 2)
                });
                self.turns_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Annotation-rate metric counts the IDENTITY layer only
                // (§6 ruling) — "-compatible" downgrades stay unannotated.
                if hfacts.identity.is_some() {
                    self.turns_with_harness
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let harness_candidates = hfacts
                    .candidates
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                let harness_enrich = parsed_req
                    .as_ref()
                    .map(|req| atg_harness::enrich(&hfacts, req))
                    .unwrap_or_default();
                let mut breakpoint = false;
                if session_id.is_empty() && stitch_eligible {
                    if let Some(messages) = tf.as_ref().and_then(|f| f.messages.as_ref()) {
                        let scope = header_get("authorization").unwrap_or_default();
                        let (synthetic, is_bp) = self.stitcher.assign(&scope, messages);
                        session_id = synthetic;
                        session_source = "stitcher".to_string();
                        breakpoint = is_bp;
                        session_synthetic = !session_id.is_empty();
                    }
                }
                // §E ruling: synthetic sessions stay out of the hit-rate
                // numerator (they are fallbacks, not observed identifiers).
                if !session_id.is_empty() && !session_synthetic {
                    self.turns_with_session
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let raw_request = self.cap.bound(&ctx.req_buf);
                let raw_response = self.cap.bound(&ctx.resp_buf);
                // Drain truncation marker (v0.3.6): raw drain capture stops at
                // the cap while the stream itself ran on — the marker reports
                // the TRUE original size (captured head + dropped overflow).
                let raw_response = if ctx.drain_overflow > 0 {
                    format!(
                        "{raw_response}[truncated:original_bytes={},captured_bytes={}]",
                        // Counters: saturating form (lint-explicit; overflow
                        // needs a >usize record which the capture cap excludes).
                        ctx.resp_buf
                            .len()
                            .saturating_add(ctx.drain_overflow as usize),
                        ctx.resp_buf.len()
                    )
                } else {
                    raw_response
                };
                if unpack::looks_like_sse(&ctx.resp_content_type) {
                    // One traversal fills text/usage/tool_calls/error. A
                    // drained turn (v0.3.6) replays its incrementally-fed
                    // accumulator instead — complete final_output/usage even
                    // when the raw capture hit the cap.
                    let (final_output, usage, tool_calls, error, frame_errors) =
                        match ctx.drain_acc.take() {
                            Some(acc) => {
                                let out = crate::engine::drain_finish(
                                    acc,
                                    matched.descriptor,
                                    &mut ctx.drain_tail,
                                );
                                (out.text, out.usage, out.tools, out.error, out.frame_errors)
                            }
                            None => unpack::reassemble_sse(protocol, &ctx.resp_buf),
                        };
                    if frame_errors > 0 {
                        let total = self.failed_frames.fetch_add(
                            u64::from(frame_errors),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        // Sampled: first occurrence + every 10th cumulative.
                        if total == 0 || total.is_multiple_of(10) {
                            eprintln!(
                                "ATG: SSE unpack frame_errors={frame_errors} cumulative={}",
                                total.saturating_add(u64::from(frame_errors))
                            );
                        }
                    }
                    let user_input = tf
                        .as_ref()
                        .map(|f| f.user_input.clone())
                        .unwrap_or_default();
                    let model_name = tf
                        .as_ref()
                        .map(|f| f.model_name.clone())
                        .unwrap_or_default();
                    let user_id = tf.as_ref().map(|f| f.user_id.clone()).unwrap_or_default();
                    ctx.end_ns = now_ns();
                    self.push_record(atg_model::TurnRecord {
                        protocol: protocol.to_string(),
                        session_id,
                        user_input,
                        final_output,
                        raw_request,
                        raw_response,
                        tool_calls,
                        breakpoint,
                        start_ns: ctx.start_ns,
                        end_ns: ctx.end_ns,
                        usage,
                        model_name,
                        user_id,
                        harness,
                        dialect: dialect.clone(),
                        client_ua: client_ua.clone(),
                        hdr_evidence,
                        session_source,
                        api_key_fp: api_key_fp.clone(),
                        harness_candidates,
                        harness_anomaly: hfacts.protocol_anomaly,
                        harness_enrich,
                        session_synthetic,
                        completion_start_ns: ctx.first_output_ns,
                        error: error.or(http_error),
                        cancelled: client_cancelled,
                        drain_timed_out: ctx.drain_timed_out,
                        trace_tag: ctx.snap.cfg.trace.tag.clone(),
                    });
                    return;
                }
                match unpack::unpack_nonstreaming(protocol, parsed_req.as_ref(), &ctx.resp_buf) {
                    Some(mut record) => {
                        record.session_id = session_id;
                        record.breakpoint = breakpoint;
                        record.harness = harness;
                        record.dialect = dialect;
                        record.client_ua = client_ua;
                        record.hdr_evidence = hdr_evidence;
                        record.session_source = session_source;
                        record.api_key_fp = api_key_fp;
                        record.harness_candidates = harness_candidates;
                        record.harness_anomaly = hfacts.protocol_anomaly;
                        record.harness_enrich = harness_enrich;
                        record.session_synthetic = session_synthetic;
                        record.completion_start_ns = Some(ctx.start_ns);
                        record.error = record.error.take().or(http_error);
                        record.cancelled = client_cancelled;
                        record.drain_timed_out = ctx.drain_timed_out;
                        record.trace_tag = ctx.snap.cfg.trace.tag.clone();
                        record.raw_request = raw_request;
                        record.raw_response = raw_response;
                        ctx.end_ns = now_ns();
                        record.start_ns = ctx.start_ns;
                        record.end_ns = ctx.end_ns;
                        self.push_record(record);
                    }
                    None if http_error.is_some() || (ctx.relay_done && client_cancelled) => {
                        // NIT-B: a proxy-level failure (no upstream response,
                        // or an unparseable error body) previously produced NO
                        // record at all — the errored turn vanished. Emit a
                        // minimal record so the failure is observable. A
                        // relayed turn whose non-SSE body never parsed (drain
                        // truncation beyond the cap) is covered by the same
                        // arm — as a cancelled turn, not a failure.
                        ctx.end_ns = now_ns();
                        self.push_record(atg_model::TurnRecord {
                            protocol: protocol.to_string(),
                            session_id,
                            user_input: tf
                                .as_ref()
                                .map(|f| f.user_input.clone())
                                .unwrap_or_default(),
                            raw_request,
                            raw_response,
                            harness,
                            harness_candidates,
                            harness_anomaly: hfacts.protocol_anomaly,
                            harness_enrich,
                            session_synthetic,
                            hdr_evidence,
                            session_source,
                            error: http_error,
                            start_ns: ctx.start_ns,
                            end_ns: ctx.end_ns,
                            cancelled: client_cancelled,
                            drain_timed_out: ctx.drain_timed_out,
                            trace_tag: ctx.snap.cfg.trace.tag.clone(),
                            model_name: tf
                                .as_ref()
                                .map(|f| f.model_name.clone())
                                .unwrap_or_default(),
                            ..Default::default()
                        });
                    }
                    None => {}
                }
            }));
        }

        fn fail_to_connect(
            &self,
            session: &mut Session,
            peer: &HttpPeer,
            _ctx: &mut Self::CTX,
            e: Box<Error>,
        ) -> Box<Error> {
            let path = session.req_header().uri.path().to_string();
            let peer = format!("{peer:?}");
            let err = format!("{e:?}");
            eprintln!("GATEWAY fail_to_connect: path={path} peer={peer} error={err}");
            e
        }

        async fn fail_to_proxy(
            &self,
            session: &mut Session,
            e: &Error,
            ctx: &mut Self::CTX,
        ) -> FailToProxy {
            let path = session.req_header().uri.path().to_string();
            let err = format!("{e:?}");
            // Same classification as the logging hook: idle teardown is
            // debug-grade noise and a client cancellation is not a gateway
            // failure — neither reaches the fail_to_proxy log line.
            let downstream_sourced = *e.esource() == pingora::ErrorSource::Downstream;
            match classify_session_error(
                &err,
                downstream_sourced,
                ctx.resp_status,
                ctx.response_complete,
            ) {
                SessionErrorClass::IdleNoise => {
                    eprintln!("ATG: idle teardown after response (path={path}) — not a failure");
                }
                SessionErrorClass::ClientCancelled => {
                    eprintln!("ATG: client cancelled mid-turn (path={path})");
                }
                SessionErrorClass::ProxyError => {
                    eprintln!("GATEWAY fail_to_proxy: path={path} error={err}");
                }
            }
            let code = match e.etype() {
                pingora::HTTPStatus(code) => *code,
                _ => match e.esource() {
                    pingora::ErrorSource::Upstream => 502,
                    pingora::ErrorSource::Downstream => match e.etype() {
                        pingora::WriteError | pingora::ReadError | pingora::ConnectionClosed => 0,
                        _ => 400,
                    },
                    _ => 500,
                },
            };
            if code > 0 {
                session.respond_error(code).await.unwrap_or_else(|e| {
                    eprintln!("failed to send error response to downstream: {e}");
                });
            }
            FailToProxy {
                error_code: code,
                can_reuse_downstream: false,
            }
        }
    }

    /// The component handles the reload path updates in place after a
    /// successful `store.reload()` — shared Arcs / atomics, so no per-request
    /// snapshot plumbing is involved. Per-request reads come from the ctx
    /// snapshot; these are the component-owned currents the batcher, the
    /// stitcher and the capture bound consult without a snapshot read.
    #[derive(Clone)]
    struct HotApply {
        cap: Arc<crate::trace::capture::CaptureCap>,
        store: TraceStore,
        stitcher: Arc<crate::trace::prefix::PrefixStitcher>,
        exporter: crate::trace::export::Exporter,
    }

    impl HotApply {
        fn apply(&self, cfg: &crate::config::Config) {
            self.cap
                .set_max(cfg.capture.max_bytes.min(usize::MAX as u64) as usize);
            self.store
                .set_caps(cfg.store.max_records, cfg.store.max_bytes);
            self.stitcher
                .set_limits(cfg.stitch.capacity, cfg.stitch.ttl_ms);
            self.exporter
                .apply_target(&cfg.export.otlp_endpoint, cfg.export.max_inflight as usize);
        }
    }

    /// The SIGHUP background service: re-reads the config file in place
    /// (transactionally — a failed reload keeps serving the old values and
    /// counts the failure). The hot-apply itself rides the store's
    /// `on_reload` hook (registered in `run()`), so the swap and the
    /// component re-seed happen atomically inside `reload()` — for SIGHUP
    /// and for in-process reload callers alike.
    struct ConfigReloadService {
        store: Arc<crate::config_store::ConfigStore>,
    }

    #[async_trait]
    impl pingora::services::background::BackgroundService for ConfigReloadService {
        async fn start(&self, mut shutdown: pingora::server::ShutdownWatch) {
            use tokio::signal::unix::{signal, SignalKind};
            let mut hup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ATG: SIGHUP listener unavailable — config hot reload disabled: {e}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = hup.recv() => {
                        // The audit line (generation, applied keys,
                        // restart_required) is emitted by reload(), which
                        // also hot-applies via the registered hook.
                        let _ = self.store.reload();
                    }
                    _ = shutdown.changed() => return,
                }
            }
        }
    }

    /// Start the gateway from a loaded config store. Blocks (pingora's
    /// `run_forever`). Structural values (listen, worker threads, capture
    /// mode, export workers) are pinned here; every hot value is read per
    /// request from the store's snapshot, and SIGHUP (the
    /// `config-reload` background service) re-reads the file in place.
    pub fn run(store: Arc<crate::config_store::ConfigStore>) {
        let snap = store.snapshot();
        let cfg = &snap.cfg;
        let listen = cfg.server.listen.clone();
        // Worker-pool size (ATG#1): a structural knob, ≥ 1 by validation.
        // Raising it is NOT a free win and must not be a silent upgrade
        // (ATG#1 Specification 5): the 8-worker shape regressed multi-hop
        // client-observed TTFB p50 by ~10x while being faster at the ATG hop
        // alone. The effective value is self-attested below and in
        // /__atg/health.
        let worker_threads = cfg.server.worker_threads as usize;
        // Capture mode (structural): prod builds only ever validate to
        // "full"; bench builds may run "off" (near-pure forward, records
        // degrade to shells).
        let trace_off = cfg.trace.mode == "off";
        let http = match reqwest::Client::builder()
            // BLOCK-B (RustGate §H): ambient HTTPS_PROXY/HTTP_PROXY env vars
            // would otherwise silently hijack the LLM data path — same
            // reasoning as the exporter's client (export.rs, explicit
            // no_proxy precedent). The SNI resolve pins are per-request
            // (cached in Gateway::pinned_relay), so the base client carries
            // none.
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agent-trace-gateway: upstream HTTP client init failed: {e}");
                std::process::exit(2);
            }
        };
        // PANIC-AUDIT v0.3.8: process startup — a pingora bootstrap failure
        // is fatal by design and must abort the process (class ii).
        #[allow(clippy::unwrap_used)]
        let mut server = Server::new(Some(Opt::default())).unwrap();
        // pingora's ServerConf default is threads: 1 — the whole proxy
        // (accept plus every stream's duplex pumps) runs on one core. The
        // v0.3.11 hot-fix force-set 8; the thread count is now the
        // structural config key server.worker_threads (ATG#1 Specification
        // 5: the multi-worker shape is an explicit opt-in).
        let worker_threads = match Arc::get_mut(&mut server.configuration) {
            Some(conf) => {
                conf.threads = worker_threads;
                worker_threads
            }
            None => {
                eprintln!(
                    "ATG: could not override worker threads — configuration shared; running single-threaded"
                );
                // pingora's ServerConf default is threads: 1 — report the
                // configuration that actually runs.
                1
            }
        };
        // Self-attestation line (v0.3.12, ATG issues #2/#3, extended by the
        // v0.4.0 config contract): one line at startup that names the binary
        // (version), the build variant, the EFFECTIVE capture mode, the
        // runtime shape and the loaded FILE with its digest and generation —
        // a deployment proves its configuration from logs alone.
        eprintln!(
            "ATG: version={} variant={} trace_mode={} worker_threads={} config={} sha256={} generation={} upstream={}",
            env!("CARGO_PKG_VERSION"),
            variant_label(),
            trace_mode_label(trace_off),
            worker_threads,
            snap.path.display(),
            hex::encode(snap.sha256),
            snap.generation,
            cfg.server.upstream
        );
        server.bootstrap();
        let trace_store = TraceStore::with_caps(
            cfg.store.max_records.min(usize::MAX as u64) as usize,
            cfg.store.max_bytes.min(usize::MAX as u64) as usize,
        );
        let stitcher = Arc::new(crate::trace::prefix::PrefixStitcher::with_limits(
            cfg.stitch.capacity,
            cfg.stitch.ttl_ms,
        ));
        let cap = Arc::new(crate::trace::capture::CaptureCap::new(
            cfg.capture.max_bytes.min(usize::MAX as u64) as usize,
        ));
        let exporter = crate::trace::export::Exporter::start(
            &cfg.export.otlp_endpoint,
            cfg.export.max_inflight as usize,
            cfg.export.workers as usize,
        );
        let hot = HotApply {
            cap: cap.clone(),
            store: trace_store.clone(),
            stitcher: stitcher.clone(),
            exporter: exporter.clone(),
        };
        let gateway = Gateway {
            config_store: store.clone(),
            trace_off,
            listen: listen.clone(),
            http,
            pinned_relay: parking_lot::RwLock::new(None),
            failed_frames: std::sync::atomic::AtomicU64::new(0),
            turns_total: std::sync::atomic::AtomicU64::new(0),
            loose_path_matches: std::sync::atomic::AtomicU64::new(0),
            turns_with_session: std::sync::atomic::AtomicU64::new(0),
            turns_with_harness: std::sync::atomic::AtomicU64::new(0),
            hdr_evidence_dropped: std::sync::atomic::AtomicU64::new(0),
            worker_threads,
            inflight: std::sync::atomic::AtomicU64::new(0),
            inflight_high_water: std::sync::atomic::AtomicU64::new(0),
            awaiting_upstream: std::sync::atomic::AtomicU64::new(0),
            stage_wait_upstream: crate::metrics::Histogram::new(),
            stage_time_to_first_byte: crate::metrics::Histogram::new(),
            stage_delivery: crate::metrics::Histogram::new(),
            stage_finalize: crate::metrics::Histogram::new(),
            store: trace_store,
            stitcher,
            cap,
            exporter,
        };
        // In-process SIGHUP → store.reload() → component hot-apply. A
        // background service on the pingora server (the same shape as the
        // exporter's thread): no HTTP write endpoint, no watcher thread of
        // our own.
        // Hot-apply wiring: the reload hook carries the SIGHUP semantics —
        // every successful reload re-seeds cap/store/stitcher/exporter from
        // the new snapshot, in-process and real binary alike.
        store.set_on_reload(Box::new(move |cfg| hot.apply(cfg)));
        let reloader = ConfigReloadService {
            store: store.clone(),
        };
        server.add_service(pingora::services::background::background_service(
            "config-reload",
            reloader,
        ));
        let mut http_proxy = http_proxy(&server.configuration, gateway);
        let mut opts = pingora::apps::HttpServerOptions::default();
        opts.h2c = true;
        http_proxy.server_options = Some(opts);
        let mut svc = pingora::services::listening::Service::new(
            "agent-trace-gateway".to_string(),
            http_proxy,
        );
        svc.add_tcp(&listen);
        server.add_service(svc);
        server.run_forever();
    }

    /// Header-evidence boundary pins (issue #32): the cap counts CHARS and
    /// never splits a codepoint. Header values reaching this helper are
    /// visible ASCII by construction (`HeaderValue::to_str`), so the
    /// multibyte arm is a defensive property of the shared `client_ua` path.
    #[cfg(test)]
    mod header_evidence_tests {
        use super::*;

        #[test]
        fn truncate_counts_chars_on_a_codepoint_boundary() {
            assert_eq!(truncate_header_value("", 256), "");
            let ascii_256 = "a".repeat(256);
            assert_eq!(truncate_header_value(&ascii_256, 256), ascii_256);
            let ascii_257 = "a".repeat(257);
            assert_eq!(truncate_header_value(&ascii_257, 256), "a".repeat(256));
            let mixed = format!("{}{}{}", "a".repeat(255), '\u{4e2d}', 'x');
            let cut = truncate_header_value(&mixed, 256);
            assert_eq!(cut.chars().count(), 256);
            assert!(cut.ends_with('\u{4e2d}'));
            assert_eq!(cut.len(), 258, "255 ASCII + one 3-byte codepoint");
        }
    }

    #[cfg(test)]
    mod upstream_config_tests {
        use super::*;

        /// BLOCK-A config pins (RustGate §H): ATG_UPSTREAM parsing and the
        /// ATG_SNI resolve entry — the relay connects to the pinned
        /// address while presenting the SNI name.
        #[test]
        fn parse_upstream_forms() {
            assert_eq!(
                parse_upstream("1.2.3.4:8443"),
                (
                    "http".to_string(),
                    "1.2.3.4".to_string(),
                    8443,
                    String::new()
                )
            );
            assert_eq!(
                parse_upstream("https://api.example.com"),
                (
                    "https".to_string(),
                    "api.example.com".to_string(),
                    443,
                    String::new()
                )
            );
            assert_eq!(
                parse_upstream("http://127.0.0.1:17000"),
                (
                    "http".to_string(),
                    "127.0.0.1".to_string(),
                    17000,
                    String::new()
                )
            );
            // Base path + trailing slash survive (pre-relay parity).
            assert_eq!(
                parse_upstream("https://gw.example.com/llm"),
                (
                    "https".to_string(),
                    "gw.example.com".to_string(),
                    443,
                    "/llm".to_string()
                )
            );
            assert_eq!(
                parse_upstream("http://127.0.0.1:17000/"),
                (
                    "http".to_string(),
                    "127.0.0.1".to_string(),
                    17000,
                    String::new()
                )
            );
            // Bracketed IPv6 with port.
            assert_eq!(
                parse_upstream("http://[::1]:8443"),
                ("http".to_string(), "::1".to_string(), 8443, String::new())
            );
        }

        #[test]
        fn sni_resolve_entry_pins_bare_ip_https() {
            let entry = sni_resolve_entry("https://1.2.3.4:8443", Some("api.example.com"))
                .expect("bare-IP https + ATG_SNI must pin");
            assert_eq!(entry.0, "api.example.com");
            assert_eq!(
                entry.1.to_string(),
                "1.2.3.4:8443",
                "the SNI name connects to the real upstream address"
            );
        }

        #[test]
        fn sni_resolve_entry_skips_non_pin_cases() {
            // SNI name == upstream host: normal DNS applies.
            assert!(sni_resolve_entry("api.example.com:443", Some("api.example.com")).is_none());
            // No ATG_SNI: nothing to override.
            assert!(sni_resolve_entry("1.2.3.4:443", None).is_none());
            // Plain http: no TLS SNI involved.
            assert!(sni_resolve_entry("http://1.2.3.4:8443", Some("api.example.com")).is_none());
        }

        /// Build-variant attestation nails (v0.3.12, ATG issue #2): the
        /// label is a compile-time property of the binary and must match the
        /// compiled feature — health, metrics and the startup line all quote
        /// it, and measurement records cite it.
        #[test]
        fn variant_label_matches_compiled_feature() {
            #[cfg(feature = "bench-trace-mode")]
            assert_eq!(variant_label(), "bench");
            #[cfg(not(feature = "bench-trace-mode"))]
            assert_eq!(variant_label(), "prod");
        }

        /// Capture-mode self-attestation (v0.3.12, ATG issue #3): the label
        /// reports the effective mode.
        #[test]
        fn trace_mode_labels() {
            assert_eq!(trace_mode_label(false), "full");
            assert_eq!(trace_mode_label(true), "off");
        }
    }
}
