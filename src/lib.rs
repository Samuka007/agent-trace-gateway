// agent-trace-gateway library: protocol interpretation + gateway app.
pub mod engine;
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
    use std::time::Duration;

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
        pub upstream: String,
        /// Precomputed relay URL base (scheme://url-host:port). On https
        /// with ATG_SNI the URL host IS the SNI name (the client's
        /// resolve() entry pins it to the real address — BLOCK-A).
        pub upstream_base: String,
        /// Upstream HTTP client for the owned relay (v0.3.6): pingora's
        /// response pump structurally aborts the upstream the moment the
        /// downstream dies, which makes a client-disconnect policy
        /// (drain switch) unenforceable inside the ProxyHttp hooks — so
        /// LLM API requests are relayed by the gateway itself instead.
        pub http: reqwest::Client,
        /// ATG_DRAIN_ON_CANCEL (default false): after the client
        /// disconnects mid-response, keep consuming the upstream stream to
        /// its natural end (sub2api-class upstreams bill the completion
        /// regardless — the trace gets the full content + usage). false =
        /// abort the upstream immediately (no wasted tokens).
        pub drain_on_cancel: bool,
        /// ATG_DRAIN_TIMEOUT_SECS (default 60): the drain window; an
        /// upstream stream that has not finished within it is abandoned
        /// (drain_timed_out marker, partial content recorded).
        pub drain_timeout: Duration,
        /// ATG_MAX_WS_FRAME_PAYLOAD (bytes; 0 = unlimited, the default): the
        /// optional single-frame refusal cap for the WS frame parser.
        pub ws_max_frame_payload: usize,
        pub store: TraceStore,
        pub stitcher: crate::trace::prefix::PrefixStitcher,
        pub cap: crate::trace::capture::CaptureCap,
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
    }

    impl Gateway {
        fn push_record(&self, record: atg_model::TurnRecord) {
            self.store.push(record.clone());
            self.exporter.submit(&record);
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
            tokio::time::Instant::now() + self.drain_timeout
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
            // The URL base carries the SNI name on https (BLOCK-A: the
            // client's resolve() entry pins it to the real address); on
            // http it is the upstream host and this Host header carries the
            // ATG_SNI override — pump parity.
            let url = format!("{}{}", self.upstream_base, path_and_query);
            let host = std::env::var("ATG_SNI").ok().filter(|s| !s.is_empty());
            let mut out = self.http.request(method, &url);
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
                if !self.drain_on_cancel {
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
                                    if !self.drain_on_cancel {
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
                        if !self.drain_on_cancel {
                            break; // abort: dropping `resp` closes the upstream
                        }
                        drain_deadline = Some(self.begin_drain(ctx, d));
                    }
                } else if self.drain_on_cancel {
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
            ctx.relay_done = true;
        }
    }

    /// Salted API-credential fingerprint: sha256(salt || key), first 16
    /// hex chars. The salt defaults to a compile-time value and can be
    /// overridden via ATG_APIKEY_SALT (rotating the salt invalidates
    /// cross-version correlation but never exposes the key). The raw key
    /// is dropped immediately — no log, record or export path ever sees
    /// the plaintext.
    pub fn api_key_fp(key: &str) -> String {
        use sha2::Digest;
        let salt = std::env::var("ATG_APIKEY_SALT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "atg-apikey-fp-salt-v1".to_string());
        let mut hasher = sha2::Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update(key.as_bytes());
        hex::encode(hasher.finalize())[..16].to_string()
    }

    /// Extract the request's API credential and fingerprint it:
    /// anthropic.messages -> x-api-key header; openai.* -> Authorization
    /// Bearer. None when the request carried no credential (internal
    /// probes) — no error, no field.
    fn request_api_key_fp(protocol: &str, header_get: &dyn Fn(&str) -> Option<String>) -> String {
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
            Some(k) if !k.trim().is_empty() => api_key_fp(k.trim()),
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

    /// ATG_MAX_WS_FRAME_PAYLOAD parse (bytes): absent → (0, false) — the
    /// default is UNLIMITED; unparsable or 0 → fall back to 0 with the
    /// startup-log flag; a positive value enables the refusal cap.
    fn parse_ws_frame_cap(env: Option<&str>) -> (usize, bool) {
        match env {
            None => (0, false),
            Some(v) => match v.trim().parse::<usize>() {
                Ok(n) if n > 0 => (n, false),
                _ => (0, true),
            },
        }
    }

    /// Parsed ATG_UPSTREAM: (scheme, host, port, base_path). "host:port"
    /// defaults to http with the port present; scheme prefixes override; a
    /// missing port defaults to 80/443 by scheme. An optional base path is
    /// preserved (leading '/', trailing slashes trimmed) — the relay's
    /// earlier construction only trimmed trailing slashes, and base-path
    /// upstreams must keep resolving to the same URLs.
    fn parse_upstream(upstream: &str) -> (String, String, u16, String) {
        let (scheme, rest) = match upstream.split_once("://") {
            Some((s, r)) => (s.to_ascii_lowercase(), r),
            None => ("http".to_string(), upstream),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
            None => (rest, String::new()),
        };
        let default_port = if scheme == "https" { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            // Bracketed IPv6: [::1] or [::1]:8443.
            match v6.split_once(']') {
                Some((h, tail)) => (
                    h,
                    tail.strip_prefix(':')
                        .and_then(|p| p.parse::<u16>().ok())
                        .unwrap_or(default_port),
                ),
                None => (v6, default_port),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                    (h, p.parse::<u16>().unwrap_or(default_port))
                }
                _ => (authority, default_port),
            }
        };
        (scheme, host.to_string(), port, path)
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
            Ctx {
                req_buf: Vec::new(),
                resp_buf: Vec::new(),
                resp_content_type: String::new(),
                ws_client_parser: atg_protocol::openai::live::WsFrameParser::with_max_payload(
                    true,
                    self.ws_max_frame_payload,
                ),
                ws_server_parser: atg_protocol::openai::live::WsFrameParser::with_max_payload(
                    false,
                    self.ws_max_frame_payload,
                ),
                ws_turn: atg_protocol::openai::live::WsTurnState::default(),
                start_ns: now_ns(),
                end_ns: 0,
                first_output_ns: None,
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
            _ctx: &mut Self::CTX,
        ) -> Result<Box<HttpPeer>> {
            let tls = self.upstream.starts_with("https://");
            let host = self
                .upstream
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            // ATG_SNI overrides SNI so an upstream given as a bare IP can still
            // complete TLS with the correct hostname.
            let sni = std::env::var("ATG_SNI")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| host.split(':').next().unwrap_or("").to_string());
            Ok(Box::new(HttpPeer::new(host, tls, sni)))
        }

        // Rewrite the Host header so the real upstream routes the request
        // correctly. Uses ATG_SNI when set, else the upstream host.
        async fn upstream_request_filter(
            &self,
            _session: &mut Session,
            upstream_request: &mut pingora::http::RequestHeader,
            _ctx: &mut Self::CTX,
        ) -> Result<()> {
            let default_host = self
                .upstream
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string();
            let host = std::env::var("ATG_SNI")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or(default_host);
            let _ = upstream_request.insert_header(http::header::HOST, host);
            Ok(())
        }

        // Control endpoint: dump collected turn records as JSON.
        async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
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
                let (exported, failed, dropped, panicked) = self.exporter.health.snapshot();
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
                    "exported": exported,
                    "failed": failed,
                    "dropped": dropped,
                    "panicked": panicked,
                    "store_dropped": self.store.dropped(),
                    "failed_frames": failed_frames,
                    "turns_total": turns_total,
                    "loose_path_matches": loose_path_matches,
                    "turns_with_session": turns_with_session,
                    "turns_with_harness": turns_with_harness
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
            if self.drain_on_cancel && !is_ws_upgrade {
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
                                self.ws_max_frame_payload,
                            );
                        ctx.ws_turn = atg_protocol::openai::live::WsTurnState::default();
                    }
                }
            } else if let Some(b) = body {
                ctx.req_buf.extend_from_slice(b);
            }
            Ok(())
        }

        async fn upstream_response_filter(
            &self,
            _session: &mut Session,
            resp: &mut pingora::http::ResponseHeader,
            ctx: &mut Self::CTX,
        ) -> Result<()> {
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
                                self.ws_max_frame_payload,
                            );
                        ctx.ws_turn = atg_protocol::openai::live::WsTurnState::default();
                    }
                }
            } else if let Some(b) = body {
                if ctx.resp_buf.is_empty() && !b.is_empty() {
                    ctx.first_output_ns = Some(now_ns());
                }
                ctx.resp_buf.extend_from_slice(b);
            }
            Ok(None)
        }

        async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
            let path = session.req_header().uri.path();
            let Some(matched) = detect_path_fail_open(path) else {
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
                    .map(|u| match u.char_indices().nth(256) {
                        Some((i, _)) => u[..i].to_string(),
                        None => u.to_string(),
                    })
                    .unwrap_or_default();
                // Salted API-credential fingerprint (correlation without the
                // key; plaintext never enters any downstream path).
                let api_key_fp = request_api_key_fp(protocol, &header_get);
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
                        api_key_fp: api_key_fp.clone(),
                        harness_candidates,
                        harness_anomaly: hfacts.protocol_anomaly,
                        harness_enrich,
                        session_synthetic,
                        completion_start_ns: ctx.first_output_ns,
                        error: error.or(http_error),
                        cancelled: client_cancelled,
                        drain_timed_out: ctx.drain_timed_out,
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
                        record.api_key_fp = api_key_fp;
                        record.harness_candidates = harness_candidates;
                        record.harness_anomaly = hfacts.protocol_anomaly;
                        record.harness_enrich = harness_enrich;
                        record.session_synthetic = session_synthetic;
                        record.completion_start_ns = Some(ctx.start_ns);
                        record.error = record.error.take().or(http_error);
                        record.cancelled = client_cancelled;
                        record.drain_timed_out = ctx.drain_timed_out;
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
                            error: http_error,
                            start_ns: ctx.start_ns,
                            end_ns: ctx.end_ns,
                            cancelled: client_cancelled,
                            drain_timed_out: ctx.drain_timed_out,
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

    /// Start the gateway on `listen`, forwarding to `upstream`. Blocks.
    /// `upstream` accepts "host:port", "http://host:port" or "https://host:port".
    pub fn run(listen: &str, upstream: &str) {
        // Drain switch (v0.3.6): per-upstream client-disconnect policy.
        // ATG_DRAIN_ON_CANCEL: truthy (1/true/yes/on) = keep consuming the
        // upstream stream after the client disconnects; default = abort.
        let drain_on_cancel = std::env::var("ATG_DRAIN_ON_CANCEL")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        // ATG_DRAIN_TIMEOUT_SECS: drain window (default 60s).
        let drain_timeout = std::env::var("ATG_DRAIN_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(60);
        let sni = std::env::var("ATG_SNI").ok().filter(|s| !s.is_empty());
        // BLOCK-A (RustGate §H): https bare-IP upstreams — URLs carry the
        // SNI name, the resolve entry pins it to the real address.
        let upstream_base = match sni_resolve_entry(upstream, sni.as_deref()) {
            Some((name, _addr)) => {
                let (scheme, _, port, path) = parse_upstream(upstream);
                format!("{scheme}://{name}:{port}{path}")
            }
            None => {
                let (scheme, host, port, path) = parse_upstream(upstream);
                format!("{scheme}://{host}:{port}{path}")
            }
        };
        let mut builder = reqwest::Client::builder()
            // BLOCK-B (RustGate §H): ambient HTTPS_PROXY/HTTP_PROXY env vars
            // would otherwise silently hijack the LLM data path — same
            // reasoning as the exporter's client (export.rs, explicit
            // no_proxy precedent).
            .no_proxy();
        if let Some((name, addr)) = sni_resolve_entry(upstream, sni.as_deref()) {
            builder = builder.resolve(&name, addr);
        }
        let http = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agent-trace-gateway: upstream HTTP client init failed: {e}");
                std::process::exit(2);
            }
        };
        let (ws_max_frame_payload, ws_cap_fallback) =
            parse_ws_frame_cap(std::env::var("ATG_MAX_WS_FRAME_PAYLOAD").ok().as_deref());
        if ws_cap_fallback {
            eprintln!("ATG: ATG_MAX_WS_FRAME_PAYLOAD invalid — frame cap disabled (unlimited)");
        }
        // PANIC-AUDIT v0.3.8: process startup — a pingora bootstrap failure
        // is fatal by design and must abort the process (class ii).
        #[allow(clippy::unwrap_used)]
        let mut server = Server::new(Some(Opt::default())).unwrap();
        server.bootstrap();
        let gateway = Gateway {
            upstream: upstream.to_string(),
            upstream_base,
            ws_max_frame_payload,
            http,
            drain_on_cancel,
            drain_timeout: Duration::from_secs(drain_timeout),
            failed_frames: std::sync::atomic::AtomicU64::new(0),
            turns_total: std::sync::atomic::AtomicU64::new(0),
            loose_path_matches: std::sync::atomic::AtomicU64::new(0),
            turns_with_session: std::sync::atomic::AtomicU64::new(0),
            turns_with_harness: std::sync::atomic::AtomicU64::new(0),
            store: TraceStore::new(),
            stitcher: crate::trace::prefix::PrefixStitcher::new(),
            cap: crate::trace::capture::CaptureCap::new(),
            exporter: crate::trace::export::Exporter::start(
                std::env::var("ATG_OTLP_ENDPOINT").ok(),
            ),
        };
        let mut http_proxy = http_proxy(&server.configuration, gateway);
        let mut opts = pingora::apps::HttpServerOptions::default();
        opts.h2c = true;
        http_proxy.server_options = Some(opts);
        let mut svc = pingora::services::listening::Service::new(
            "agent-trace-gateway".to_string(),
            http_proxy,
        );
        svc.add_tcp(listen);
        server.add_service(svc);
        server.run_forever();
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

        /// ATG_MAX_WS_FRAME_PAYLOAD parse (v0.3.8 final ruling): absent or
        /// unset → 0 = UNLIMITED (no frame refusal; memory grows with the
        /// bytes actually received); unparsable or 0 → falls back to 0 with
        /// the startup-log flag; a positive value enables the cap.
        #[test]
        fn parse_ws_frame_cap_defaults_to_unlimited() {
            assert_eq!(
                parse_ws_frame_cap(None),
                (0, false),
                "absent env = unlimited"
            );
            assert_eq!(parse_ws_frame_cap(Some("65536")), (65536, false));
            assert_eq!(
                parse_ws_frame_cap(Some("0")),
                (0, true),
                "explicit 0 falls back with the startup log"
            );
            assert_eq!(
                parse_ws_frame_cap(Some("garbage")),
                (0, true),
                "unparsable falls back with the startup log"
            );
        }
    }
}
