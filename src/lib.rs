// agent-trace-gateway library: protocol interpretation + gateway app.
pub mod engine;
pub mod trace;

pub mod gateway_app {
    use async_trait::async_trait;
    use bytes::Bytes;
    use pingora::http::ResponseHeader;
    use pingora::prelude::*;
    use pingora::proxy::{http_proxy, FailToProxy, ProxyHttp, Session};
    use pingora::upstreams::peer::HttpPeer;

    use crate::trace::store::TraceStore;
    use crate::trace::unpack;

    pub struct Gateway {
        pub upstream: String,
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
    }

    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
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
    }

    #[async_trait]
    impl ProxyHttp for Gateway {
        type CTX = Ctx;

        fn new_ctx(&self) -> Self::CTX {
            Ctx {
                req_buf: Vec::new(),
                resp_buf: Vec::new(),
                resp_content_type: String::new(),
                ws_client_parser: atg_protocol::openai::live::WsFrameParser::new(true),
                ws_server_parser: atg_protocol::openai::live::WsFrameParser::new(false),
                ws_turn: atg_protocol::openai::live::WsTurnState::default(),
                start_ns: now_ns(),
                end_ns: 0,
                first_output_ns: None,
                resp_status: 0,
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
        async fn request_filter(
            &self,
            session: &mut Session,
            _ctx: &mut Self::CTX,
        ) -> Result<bool> {
            if session.req_header().uri.path() == "/__atg/records" {
                let records = self.store.snapshot();
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
                let (exported, failed, dropped) = self.exporter.health.snapshot();
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
                    for payload in ctx.ws_client_parser.push(b) {
                        ctx.ws_turn.apply_client_frame(&payload);
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
            _end: bool,
            ctx: &mut Self::CTX,
        ) -> Result<Option<std::time::Duration>> {
            if session.was_upgraded() {
                if let Some(b) = body {
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
            let Some(matched) = atg_protocol::ProtocolDescriptor::detect_path(path) else {
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
            let http_error = if let Some(e) = e {
                Some(format!("proxy_error: {e:?}"))
            } else if ctx.resp_status >= 400 {
                Some(format!("http_status: {}", ctx.resp_status))
            } else {
                None
            };
            let header_get = |name: &str| -> Option<String> {
                session
                    .req_header()
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            // Single parse of req_buf — shared with every extractor (C14).
            let parsed_req: Option<serde_json::Value> = unpack::parse_body(&ctx.req_buf);
            // Harness attribution (orthogonal to session extraction; the
            // user-agent is the canary-path signal — the main OTLP path
            // records no UA).
            let ua = header_get("user-agent");
            let hfacts =
                atg_harness::identify(protocol, parsed_req.as_ref(), ua.as_deref(), &header_get);
            let harness = if hfacts.name == atg_harness::UNKNOWN {
                String::new()
            } else {
                hfacts.name.to_string()
            };
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
            if !harness.is_empty() {
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
            if unpack::looks_like_sse(&ctx.resp_content_type) {
                // One traversal of resp_buf fills text/usage/tool_calls/error.
                let (final_output, usage, tool_calls, error, frame_errors) =
                    unpack::reassemble_sse(protocol, &ctx.resp_buf);
                if frame_errors > 0 {
                    let total = self.failed_frames.fetch_add(
                        u64::from(frame_errors),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    // Sampled: first occurrence + every 10th cumulative.
                    if total == 0 || total % 10 == 0 {
                        eprintln!(
                            "ATG: SSE unpack frame_errors={frame_errors} cumulative={}",
                            total + u64::from(frame_errors)
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
                    harness_candidates,
                    harness_anomaly: hfacts.protocol_anomaly,
                    harness_enrich,
                    session_synthetic,
                    completion_start_ns: ctx.first_output_ns,
                    error: error.or(http_error),
                });
                return;
            }
            match unpack::unpack_nonstreaming(protocol, parsed_req.as_ref(), &ctx.resp_buf) {
                Some(mut record) => {
                    record.session_id = session_id;
                    record.breakpoint = breakpoint;
                    record.harness = harness;
                    record.harness_candidates = harness_candidates;
                    record.harness_anomaly = hfacts.protocol_anomaly;
                    record.harness_enrich = harness_enrich;
                    record.session_synthetic = session_synthetic;
                    record.completion_start_ns = Some(ctx.start_ns);
                    record.error = record.error.take().or(http_error);
                    record.raw_request = raw_request;
                    record.raw_response = raw_response;
                    ctx.end_ns = now_ns();
                    record.start_ns = ctx.start_ns;
                    record.end_ns = ctx.end_ns;
                    self.push_record(record);
                }
                None if http_error.is_some() => {
                    // NIT-B: a proxy-level failure (no upstream response,
                    // or an unparseable error body) previously produced NO
                    // record at all — the errored turn vanished. Emit a
                    // minimal record so the failure is observable.
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
                        model_name: tf
                            .as_ref()
                            .map(|f| f.model_name.clone())
                            .unwrap_or_default(),
                        ..Default::default()
                    });
                }
                None => {}
            }
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
            _ctx: &mut Self::CTX,
        ) -> FailToProxy {
            let path = session.req_header().uri.path().to_string();
            let err = format!("{e:?}");
            eprintln!("GATEWAY fail_to_proxy: path={path} error={err}");
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
        let mut server = Server::new(Some(Opt::default())).unwrap();
        server.bootstrap();
        let gateway = Gateway {
            upstream: upstream.to_string(),
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
}
